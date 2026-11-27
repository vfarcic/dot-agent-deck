import "../styles.css";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { isIssueUrl, issueLabels, referenceUrl } from "../lib/agentCardsTree";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { OpenCardReference, type CardReferenceTarget } from "../lib/prBrowser";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardCheckedRef, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { OverviewWithCardPane } from "../test/cardPane";
import { needsYouOnlyChecked, pressNeedsYouOnly } from "../test/needsYouMenu";
import { AgentOverview } from "./AgentOverview";

/*
 * Issue #2418 — the Cards view as a tree of root agents with compact, checked
 * cards (`desktop/agent-cards/009`–`017`).
 *
 * Written from the plan's D6–D8, not from the components. The daemon builds
 * the tree (`parent`), the roll-ups (`rollup`) and the checked references
 * (`checked_refs`); the desktop only draws them (CLAUDE.md rule 18). So the
 * fixtures carry roll-ups a client could NOT recompute from the entries on
 * screen (an update changes a roll-up and nothing else), and checked results
 * that contradict the agent's claim, and the tests assert what is drawn.
 *
 * The storage key `dot-agent-deck.cards.open` is written out rather than
 * imported, so a renamed constant fails too.
 */

const MODE_KEY = "dot-agent-deck.overview.mode";
const OPEN_KEY = "dot-agent-deck.cards.open";
const INSTANCE = "daemon-instance-tree";

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
    actions: [],
    primary_action_id: "open",
    facts: { source: "deck", pull_request: null },
    revision: 1,
    updated_at: "2026-11-20T00:00:00Z",
    ...overrides,
  };
}

const key = (agentId: string) => `pane-${agentId}/${agentId}`;
const DISPATCHED = { dispatcher: { key: key("1"), name: "queue" } };

const PR_MERGED: AgentCardCheckedRef = {
  field: "pull-request", kind: "pull_request", claim: "#24180", repo: "vfarcic/dot-agent-deck", number: 24180, url: "https://github.com/vfarcic/dot-agent-deck/pull/24180",
  result: { status: "checked", state: "merged", ci: { state: "failing", passing: 3, failing: 1, pending: 0, skipped: 0 }, review: "approved", title: "Cards, first slice", checked_at: "2026-11-20T10:00:00Z", stale: false },
};
const ISSUE_LABELS = [
  { name: "bug", color: "d73a4a" },
  { name: "neutral", color: "not-a-colour" },
  { name: "help wanted", color: "008672" },
  { name: "documentation", color: "0075ca" },
  { name: "long-tail-label", color: "bfdadc" },
];
const ISSUE_CLOSED: AgentCardCheckedRef = {
  field: "issue", kind: "issue", claim: "o/r#3", repo: "o/r", number: 3, url: "https://github.com/o/r/issues/3",
  result: { status: "checked", state: "closed", title: "An issue", checked_at: "2026-11-20T10:00:00Z", stale: false, labels: ISSUE_LABELS },
};
const PR_PENDING: AgentCardCheckedRef = { field: "pull-request", kind: "pull_request", claim: "#7", number: 7, result: { status: "pending" } };
const PR_NOT_FOUND: AgentCardCheckedRef = { field: "pr-two", kind: "pull_request", claim: "o/r#8", repo: "o/r", number: 8, url: "https://github.com/o/r/pull/8", result: { status: "unavailable", reason: "not_found", detail: "GraphQL: Could not resolve to a PullRequest" } };
const ISSUE_UNRECOGNISED: AgentCardCheckedRef = { field: "issue-two", kind: "issue", claim: "see the notes", result: { status: "unavailable", reason: "unrecognised", detail: "not a GitHub pull request or issue reference" } };
const PR_STALE: AgentCardCheckedRef = {
  field: "pr-three", kind: "pull_request", claim: "#9", repo: "o/r", number: 9, url: "https://github.com/o/r/pull/9",
  result: { status: "checked", state: "open", checked_at: "2026-11-20T09:00:00Z", stale: true },
};

/*
 * The tree, in the daemon's order (preorder):
 *   queue (dispatcher)            idle,   4 under it, worst waiting_for_input, 1 needs you
 *     lead (orchestrator)         idle,   2 under it, worst waiting_for_input, 1 needs you
 *       coder (role)              working
 *       tester (role)             waiting_for_input, needs you
 *     unit (agent)                unknown, with checked references
 *   solo (agent)                  error, no children
 *   triage (dispatcher)           idle,   1 under it, worst blocked, 1 needs you
 *     stuck (agent)               blocked, needs you
 */
const QUEUE = entry("1", "queue", {
  report: { source: "agent", status: "working", summary: "SAID-QUEUE-SUMMARY", published_at: "2026-11-20T00:00:00Z", revision: 1 },
  fields: [
    { name: "progress", label: "Progress", description: null, type: "text", icon: null, placement: "card", custom: false, source: "agent", value: "PROGRESS-QUEUE" },
    { name: "next-step", label: "Next step", description: null, type: "text", icon: null, placement: "card", custom: false, source: "agent", value: "NEXT-STEP-QUEUE" },
  ],
  rollup: { total: 4, worst: "waiting_for_input", counts: { waiting_for_input: 1, working: 1, idle: 2 }, needs_you: 1 },
});
const LEAD = entry("2", "lead", {
  parent: key("1"), relations: { ...DISPATCHED, orchestration: { id: "o-1", label: "team", role: "lead", is_orchestrator: true } },
  rollup: { total: 2, worst: "waiting_for_input", counts: { waiting_for_input: 1, working: 1 }, needs_you: 1 },
});
const CODER = entry("3", "coder", { parent: key("2"), status: "working", agent_type: "codex", relations: { orchestration: { id: "o-1", label: "team", role: "coder", is_orchestrator: false } } });
const TESTER = entry("4", "tester", { parent: key("2"), status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"], relations: { orchestration: { id: "o-1", label: "team", role: "tester", is_orchestrator: false } } });
const UNIT = entry("5", "unit", { parent: key("1"), status: "unknown", agent_type: "open_code", relations: DISPATCHED, checked_refs: [PR_MERGED, ISSUE_CLOSED, PR_PENDING, PR_NOT_FOUND, ISSUE_UNRECOGNISED, PR_STALE] });
const SOLO = entry("6", "solo", { status: "error" });
const TRIAGE = entry("7", "triage", { rollup: { total: 1, worst: "blocked", counts: { blocked: 1 }, needs_you: 1 } });
const STUCK = entry("8", "stuck", { parent: key("7"), status: "blocked", needs_you: true, needs_you_reasons: ["blocked"], relations: { dispatcher: { key: key("7"), name: "triage" } } });

const TREE = [QUEUE, LEAD, CODER, TESTER, UNIT, SOLO, TRIAGE, STUCK];

function fleet(entries: AgentCardEntry[]): DeckFleet {
  const deck = createFixtureFleet("fleet")[0];
  const agents = entries.map((card) => createFixtureStartedAgent({ id: card.agent_id, daemonId: FIXTURE_DAEMON_ID, displayName: card.name, command: "claude", cwd: "/w" }));
  return [{ ...deck, agents }];
}

function snapshot(entries: AgentCardEntry[], { tree = true, revision = 1 }: { tree?: boolean; revision?: number } = {}): AgentCardsSnapshot {
  return { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision, entries, catalogErrors: [], tree };
}

function runtime(entries: AgentCardEntry[] = TREE, options: { tree?: boolean } = {}): DeckRuntimeState {
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
    agentCards: { [FIXTURE_DAEMON_ID]: snapshot(entries, options) },
    invokeCardAction: vi.fn(),
  } as unknown as DeckRuntimeState;
}

const openReference = vi.fn<(target: CardReferenceTarget) => string | undefined>(() => undefined);

function cardsView(live: DeckRuntimeState = runtime()) {
  window.localStorage.setItem(MODE_KEY, "cards");
  const view = render(<OpenCardReference.Provider value={openReference}><OverviewWithCardPane runtime={live} /></OpenCardReference.Provider>);
  return {
    ...view,
    update: (next: DeckRuntimeState) => view.rerender(<OpenCardReference.Provider value={openReference}><OverviewWithCardPane runtime={next} /></OpenCardReference.Provider>),
  };
}

/** The cards on screen, by the key the daemon gave each, in document order. */
const shownKeys = () => [...document.querySelectorAll("article[data-card-key]")].map((node) => node.getAttribute("data-card-key"));
const cardOf = (cardKey: string) => {
  const found = document.querySelector(`article[data-card-key="${cardKey}"]`);
  if (!(found instanceof HTMLElement)) throw new Error(`no card ${cardKey} on screen`);
  return found;
};
const toggle = (card: AgentCardEntry) => within(cardOf(card.key)).getByTestId("agent-card-toggle");
const roots = () => [...document.querySelectorAll("article[data-card-key][data-depth='0']")].map((node) => node.getAttribute("data-card-key"));

beforeEach(() => {
  window.localStorage.clear();
  openReference.mockClear();
});
afterEach(() => {
  act(() => setDashboardFilter(clearDashboardFilter()));
  vi.restoreAllMocks();
  window.localStorage.clear();
});

/** A placed key as the open cards are remembered under: deck, daemon instance, card key. */
const placed = (card: AgentCardEntry, instance = INSTANCE) => `${FIXTURE_DAEMON_ID}\u0000${instance}\u0000${card.key}`;

describe("desktop/agent-cards — the Cards tree (issue #2418)", () => {
  /// Scenario: open the Cards view on a daemon list whose entries carry parent links. Only the three roots show; opening the dispatcher shows exactly its two children in the daemon's order, opening the orchestrator under it shows its two roles one level deeper, and closing the dispatcher takes its whole tree off screen.
  it("desktop/agent-cards/009: only roots show at first, and opening a card shows exactly its children, to any depth", () => {
    cardsView();
    expect(roots()).toEqual([QUEUE.key, SOLO.key, TRIAGE.key]);
    expect(shownKeys()).toEqual([QUEUE.key, SOLO.key, TRIAGE.key]);
    // A card with nothing under it has no toggle.
    expect(within(cardOf(SOLO.key)).queryByTestId("agent-card-toggle")).toBeNull();
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("false");
    expect(toggle(QUEUE).getAttribute("aria-label")).toBe("Open queue's tree");

    toggle(QUEUE).focus();
    expect(document.activeElement).toBe(toggle(QUEUE));
    fireEvent.click(toggle(QUEUE), { detail: 0 });
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("true");
    const band = screen.getByTestId("agent-cards-band");
    expect(band.getAttribute("data-root-key")).toBe(QUEUE.key);
    expect([...band.querySelectorAll("article[data-card-key]")].map((node) => node.getAttribute("data-card-key"))).toEqual([LEAD.key, UNIT.key]);
    expect(cardOf(LEAD.key).getAttribute("data-depth")).toBe("1");
    expect(roots()).toEqual([QUEUE.key, SOLO.key, TRIAGE.key]);
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();

    fireEvent.click(toggle(LEAD));
    expect([...band.querySelectorAll("article[data-card-key]")].map((node) => node.getAttribute("data-card-key"))).toEqual([LEAD.key, CODER.key, TESTER.key, UNIT.key]);
    expect(cardOf(CODER.key).getAttribute("data-depth")).toBe("2");

    fireEvent.click(toggle(QUEUE));
    expect(shownKeys()).toEqual([QUEUE.key, SOLO.key, TRIAGE.key]);
    expect(screen.queryByTestId("agent-cards-band")).toBeNull();
  });

  /// Scenario: look at the closed cards that have children. Each is coloured by the worst of its own status and its roll-up's, shows a bar of the roll-up's counts by status and "N under it · M need you" as the daemon counted them; opened, the dispatcher takes its own status's colour again.
  it("desktop/agent-cards/010: a closed card with children shows its roll-up's colour, counts bar and totals", () => {
    cardsView();
    // queue is idle itself; the worst under it is waiting_for_input.
    expect(cardOf(QUEUE.key).getAttribute("data-tone")).toBe("needs_input");
    const bar = within(cardOf(QUEUE.key)).getByTestId("agent-card-rollup-bar");
    expect(bar.getAttribute("aria-label")).toBe("1 needs input, 1 working, 2 idle");
    expect([...bar.querySelectorAll("[data-status]")].map((segment) => segment.getAttribute("data-status"))).toEqual(["needs_input", "working", "idle"]);
    expect(within(cardOf(QUEUE.key)).getByTestId("agent-card-rollup").textContent).toBe("4 under it · 1 needs you");
    expect(cardOf(TRIAGE.key).getAttribute("data-tone")).toBe("blocked");
    expect(within(cardOf(TRIAGE.key)).getByTestId("agent-card-rollup").textContent).toBe("1 under it · 1 needs you");
    // A card with no children: its own status, and no roll-up.
    expect(cardOf(SOLO.key).getAttribute("data-tone")).toBe("error");
    expect(within(cardOf(SOLO.key)).queryByTestId("agent-card-rollup")).toBeNull();

    fireEvent.click(toggle(QUEUE));
    expect(cardOf(QUEUE.key).getAttribute("data-tone")).toBe("idle");
  });

  /// Scenario: with the dispatcher's tree open, the daemon changes a roll-up and a child's status. The bar, totals and colour follow the update, and the child's status button announces its kind and current status before and after the change while the tree stays open.
  it("desktop/agent-cards/011: roll-ups and statuses update live, and the tree stays open", () => {
    const live = runtime();
    const view = cardsView(live);
    fireEvent.click(toggle(TRIAGE));
    expect(within(cardOf(STUCK.key)).getByTestId("agent-card-details")).toHaveAccessibleName(/agent.*blocked/);
    expect(cardOf(QUEUE.key).getAttribute("data-tone")).toBe("needs_input");
    const queue = { ...QUEUE, revision: 2, rollup: { total: 5, worst: "error", counts: { error: 1, working: 2, idle: 2 }, needs_you: 2 } };
    const stuck = { ...STUCK, revision: 2, status: "working", needs_you: false, needs_you_reasons: [] };
    view.update({ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([queue, LEAD, CODER, TESTER, UNIT, SOLO, TRIAGE, stuck], { revision: 2 }) } });
    expect(cardOf(QUEUE.key).getAttribute("data-tone")).toBe("error");
    expect(within(cardOf(QUEUE.key)).getByTestId("agent-card-rollup").textContent).toBe("5 under it · 2 need you");
    expect(within(cardOf(QUEUE.key)).getByTestId("agent-card-rollup-bar").getAttribute("aria-label")).toBe("1 error, 2 working, 2 idle");
    // triage's own tree is still open and its child shows its new status.
    expect(toggle(TRIAGE).getAttribute("aria-expanded")).toBe("true");
    expect(within(cardOf(STUCK.key)).getByTestId("agent-card-status").textContent).toBe("working");
    expect(within(cardOf(STUCK.key)).getByTestId("agent-card-details")).toHaveAccessibleName(/agent.*working/);
    expect(within(cardOf(STUCK.key)).getByTestId("agent-card-details")).not.toHaveAccessibleName(/blocked/);
  });

  /// Scenario: open the trees and read the cards. Each shows its name, its kind (dispatcher, orchestrator, role or agent), its status in the daemon's words and the needs-you marker, with no agent type; what the agent said, its progress and its next step are not on the card, and clicking the card body opens its agent with the Card tab, which still shows them and the agent type.
  it("desktop/agent-cards/012: a compact card shows who and what, in the daemon's words, and the Card tab keeps what the agent said", () => {
    cardsView();
    fireEvent.click(toggle(QUEUE));
    fireEvent.click(toggle(LEAD));
    const text = (card: AgentCardEntry, testId: string) => within(cardOf(card.key)).getByTestId(testId).textContent;
    expect(text(QUEUE, "agent-card-kind")).toBe("dispatcher");
    expect(text(LEAD, "agent-card-kind")).toBe("orchestrator");
    expect(text(CODER, "agent-card-kind")).toBe("role");
    expect(text(UNIT, "agent-card-kind")).toBe("agent");
    expect(text(SOLO, "agent-card-kind")).toBe("agent");
    for (const card of [QUEUE, CODER, UNIT]) {
      expect(within(cardOf(card.key)).queryByText(/^(Codex|OpenCode|Claude Code)$/)).toBeNull();
    }
    // The daemon's words, as the table and the TUI show them.
    expect(text(CODER, "agent-card-status")).toBe("working");
    expect(text(TESTER, "agent-card-status")).toBe("needs input");
    expect(text(UNIT, "agent-card-status")).toBe("idle");
    expect(text(SOLO, "agent-card-status")).toBe("error");
    expect(within(cardOf(TESTER.key)).getByTestId("agent-card-needs-you")).toBeInTheDocument();
    expect(within(cardOf(CODER.key)).queryByTestId("agent-card-needs-you")).toBeNull();
    // Said, progress and next step leave the card.
    expect(within(cardOf(QUEUE.key)).queryByText("SAID-QUEUE-SUMMARY")).toBeNull();
    expect(within(cardOf(QUEUE.key)).queryByText("PROGRESS-QUEUE")).toBeNull();
    expect(within(cardOf(QUEUE.key)).queryByText("NEXT-STEP-QUEUE")).toBeNull();
    fireEvent.click(within(cardOf(QUEUE.key)).getByTestId("agent-card-details"));
    const panel = screen.getByTestId("agent-card-panel");
    expect(panel.getAttribute("data-card-key")).toBe(QUEUE.key);
    expect(within(panel).getByText("SAID-QUEUE-SUMMARY")).toBeInTheDocument();
    expect(within(panel).getByText("PROGRESS-QUEUE")).toBeInTheDocument();
    expect(within(panel).getByText("NEXT-STEP-QUEUE")).toBeInTheDocument();
    expect(within(panel).getByText("Claude Code")).toBeInTheDocument();
  });

  /// Scenario: open the dispatcher and read the unit's card, whose agent claimed pull request #24180 and issue o/r#3. The pull request badge shows the deck's check — merged, CI failing, approved — and the issue badge shows it closed; clicking each opens its GitHub page through the app, with the checked address.
  it("desktop/agent-cards/013: checked pull request and issue badges show the deck's check and open their GitHub pages", () => {
    cardsView();
    fireEvent.click(toggle(QUEUE));
    const unit = cardOf(UNIT.key);
    const pr = within(unit).getAllByTestId("agent-card-pr")[0];
    expect(pr.getAttribute("data-pr-state")).toBe("merged");
    expect(pr.getAttribute("data-pr-checks")).toBe("failing");
    expect(pr.getAttribute("data-pr-review")).toBe("approved");
    expect(pr.getAttribute("aria-label")).toContain("#24180");
    pr.focus();
    expect(document.activeElement).toBe(pr);
    act(() => { fireEvent.click(pr, { detail: 0 }); });
    expect(openReference).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, cardKey: UNIT.key, kind: "pull_request", number: 24180, url: PR_MERGED.url });
    // A click on a badge does not open the agent's pane.
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();

    const issue = within(unit).getByTestId("agent-card-issue");
    expect(issue.getAttribute("data-issue-state")).toBe("closed");
    expect(issue.getAttribute("aria-label")).toContain("#3");
    issue.focus();
    expect(document.activeElement).toBe(issue);
    act(() => { fireEvent.click(issue, { detail: 0 }); });
    expect(openReference).toHaveBeenLastCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, cardKey: UNIT.key, kind: "issue", number: 3, url: ISSUE_CLOSED.url });
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();
  });

  /// Scenario: read an issue's labels on its compact card. The repo's valid colour decorates its label, an invalid colour uses the same neutral appearance as a missing colour, and the display-only chips are not keyboard controls.
  it("desktop/agent-cards/013: issue label chips use the repo colour and invalid colours fall back to neutral", () => {
    const reference: AgentCardCheckedRef = { ...ISSUE_CLOSED, result: { ...ISSUE_CLOSED.result, status: "checked", state: "closed", checked_at: "2026-11-20T10:00:00Z", stale: false, labels: [ISSUE_LABELS[0], ISSUE_LABELS[1], { name: "no colour" }] } };
    const card = entry("5", "unit", { checked_refs: [reference] });
    cardsView(runtime([card]));
    expect(within(cardOf(card.key)).getByText("bug")).toBeInTheDocument();
    const line = within(cardOf(card.key)).getByTestId("agent-card-issue-labels");
    const chips = within(line).getAllByTestId("agent-card-issue-label");
    expect(chips.map((chip) => chip.textContent)).toEqual(["bug", "neutral", "no colour"]);
    const decoration = (chip: HTMLElement) => [chip, ...chip.querySelectorAll<HTMLElement>("*")].map((node) => {
      const style = getComputedStyle(node);
      return [style.color, style.backgroundColor, style.borderColor].join(";");
    }).join(";");
    expect(decoration(chips[0])).toContain("rgb(215, 58, 74)"); // theme-invariant: the repo's supplied label colour is the same in either theme.
    expect(decoration(chips[1])).toBe(decoration(chips[2]));
    expect(line.querySelector("button, a, [role='button'], [tabindex='0']")).toBeNull();
  });

  /// Scenario: give the issue's label line room for two chips and the overflow count. Two labels remain visible and +3 accounts for the others, while opening the agent with its Card tab lists all five labels without an overflow cap.
  it("desktop/agent-cards/013: labels that do not fit are counted by +N and the Card tab lists them all", () => {
    // jsdom has no layout: model a 200 px label line with 60 px chips and 6 px gaps.
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-labels" ? 200 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-label" ? 60 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function left(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-label" ? [...(this.parentElement?.children ?? [])].indexOf(this) * 66 : 0; });
    const card = entry("5", "unit", { checked_refs: [ISSUE_CLOSED] });
    cardsView(runtime([card]));
    const article = cardOf(card.key);
    expect(within(article).getByText("bug")).toBeInTheDocument();
    const line = within(article).getByTestId("agent-card-issue-labels");
    const chips = within(line).getAllByTestId("agent-card-issue-label");
    expect(chips.filter((chip) => chip.getAttribute("aria-hidden") !== "true").map((chip) => chip.textContent)).toEqual(["bug", "neutral"]);
    expect(within(line).getByText("+3")).toBeInTheDocument();
    fireEvent.click(article);
    const section = within(screen.getByTestId("agent-card-panel")).getByRole("region", { name: "Pull requests and issues" });
    for (const label of ISSUE_LABELS) expect(within(section).getByText(label.name)).toBeVisible();
    expect(within(section).queryByText(/^\+\d+$/)).toBeNull();
  });

  /// Scenario: a malformed daemon sends too many issue labels or one overlong name. The helper accepts only 20 names of at most 50 characters, and both the compact card and the Card tab create chips only for those bounded labels, including hidden overflow chips.
  it.each([
    { surface: "helper", count: 100 }, { surface: "helper", count: 1 },
    { surface: "rendered card", count: 100 }, { surface: "rendered card", count: 1 },
  ])("desktop/agent-cards/013: oversized issue labels are bounded in the $surface with $count labels (R2-A2)", ({ surface, count }) => {
    const labels = Array.from({ length: count }, (_, at) => ({ name: `label-${at}-` + "x".repeat(100), color: "D73A4A" }));
    const reference: AgentCardCheckedRef = { ...ISSUE_CLOSED, result: { status: "checked", state: "closed", checked_at: "2026-11-20T10:00:00Z", stale: false, labels } };
    const accepted = issueLabels(reference);
    const expected = labels.slice(0, 20).map((label) => label.name.slice(0, 50));
    if (surface === "helper") {
      expect(accepted).toHaveLength(Math.min(count, 20));
      expect(accepted.map((label) => label.name)).toEqual(expected);
      expect(accepted.every((label) => label.color === "d73a4a")).toBe(true);
      return;
    }
    // Model the same narrow label line as the overflow scenario above.
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-labels" ? 200 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-label" ? 60 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function left(this: HTMLElement) { return this.dataset.testid === "agent-card-issue-label" ? [...(this.parentElement?.children ?? [])].indexOf(this) * 66 : 0; });
    cardsView(runtime([entry("5", "unit", { checked_refs: [reference] })]));
    const article = cardOf(key("5"));
    const chips = within(article).getAllByTestId("agent-card-issue-label");
    expect(chips).toHaveLength(Math.min(count, 20));
    expect(chips.map((chip) => chip.textContent)).toEqual(expected);
    if (count > 20) {
      expect(chips.filter((chip) => chip.getAttribute("aria-hidden") === "true")).toHaveLength(18);
      expect(within(article).getByTestId("agent-card-issue-labels-more")).toHaveTextContent("+18");
    }
    expect(article.textContent).not.toContain("label-20-");
    fireEvent.click(article);
    const section = screen.getByTestId("agent-card-panel-refs");
    const panelChips = within(section).getAllByTestId("agent-card-panel-issue-label");
    expect(panelChips).toHaveLength(Math.min(count, 20));
    expect(panelChips.map((chip) => chip.textContent)).toEqual(expected);
  });

  /// Scenario: a malformed daemon puts a usable label after 1,000 invalid entries. The desktop stops its bounded scan before that distant entry and creates no label chips for it.
  it("desktop/agent-cards/013: malformed label entries cannot make the desktop scan an unbounded tail (R2-A2)", () => {
    const labels = [...Array.from({ length: 1000 }, () => ({ name: "" })), { name: "UNBOUNDED_SCAN_SENTINEL" }];
    const reference: AgentCardCheckedRef = { ...ISSUE_CLOSED, result: { status: "checked", state: "open", checked_at: "2026-11-20T10:00:00Z", stale: false, labels } };
    expect(issueLabels(reference)).toEqual([]);
    cardsView(runtime([entry("5", "unit", { checked_refs: [reference] })]));
    expect(screen.queryAllByTestId("agent-card-issue-label")).toHaveLength(0);
    expect(screen.queryByText("UNBOUNDED_SCAN_SENTINEL")).toBeNull();
  });

  /// Scenario: read the unit's references the deck could not check, or has not yet. A pending one says it is being checked, a not-found one says "not found or private", one that is not a GitHub reference says so, each with the reason in its tooltip and no link; a stale one keeps its badge but is marked stale, with when it was last checked in its tooltip.
  /// Scenario: click the text of a reference the deck has not checked yet, and of one it could not find, on a card whose daemon already gave both an address. Neither text opens a page; each click reaches the card and opens its agent with the Card tab, as a click anywhere else on the card does.
  it("desktop/agent-cards/014: a reference shown as text, with an address, lets a click through to the card", () => {
    const pendingWithUrl: AgentCardCheckedRef = { field: "pull-request", kind: "pull_request", claim: "o/r#9", repo: "o/r", number: 9, url: "https://github.com/o/r/pull/9", result: { status: "pending" } };
    const card = entry("5", "unit", { checked_refs: [pendingWithUrl, PR_NOT_FOUND] });
    for (const status of ["pending", "unavailable"]) {
      const view = cardsView(runtime([card]));
      const reference = within(cardOf(card.key)).getAllByTestId("agent-card-ref").find((node) => node.getAttribute("data-ref-status") === status);
      if (!reference) throw new Error(`no ${status} reference`);
      expect(within(reference).queryByRole("button")).toBeNull();
      const text = reference.querySelector(".agent-card-ref-text");
      if (!(text instanceof HTMLElement)) throw new Error(`no text on the ${status} reference`);
      fireEvent.click(text);
      expect(openReference).not.toHaveBeenCalled();
      expect(screen.getByTestId("agent-card-panel").getAttribute("data-card-key")).toBe(card.key);
      view.unmount();
    }
  });

  it("desktop/agent-cards/014: pending, unavailable and stale references say so", () => {
    cardsView();
    fireEvent.click(toggle(QUEUE));
    const refs = within(cardOf(UNIT.key)).getAllByTestId("agent-card-ref");
    const byField = (field: string) => {
      const found = refs.find((node) => node.getAttribute("data-field") === field);
      if (!found) throw new Error(`no reference for ${field}`);
      return found;
    };
    // Two references come from the `pull-request` field: #24180 and the pending #7.
    const pending = refs.filter((node) => node.getAttribute("data-ref-status") === "pending");
    expect(pending).toHaveLength(1);
    expect(pending[0].textContent).toContain("#7");
    expect(pending[0].textContent).toContain("checking");
    expect(pending[0].getAttribute("title")).toMatch(/not looked .* up yet/i);

    const notFound = byField("pr-two");
    expect(notFound.getAttribute("data-ref-status")).toBe("unavailable");
    expect(notFound.textContent).toContain("not found or private");
    expect(notFound.getAttribute("title")).toContain("Could not resolve to a PullRequest");
    expect(within(notFound).queryByRole("button")).toBeNull();

    const unrecognised = byField("issue-two");
    expect(unrecognised.textContent).toContain("not a GitHub reference");
    expect(unrecognised.getAttribute("title")).toContain("see the notes");

    const stale = byField("pr-three");
    expect(stale.getAttribute("data-ref-status")).toBe("stale");
    expect(within(stale).getByTestId("agent-card-pr").getAttribute("data-pr-state")).toBe("open");
    expect(stale.textContent).toContain("stale");
    expect(stale.getAttribute("title")).toMatch(/last checked/i);
  });

  /// Scenario: press Needs you only in the top bar's ⋯ menu with every tree closed. The trees on the path to each card that needs the user open, those cards and the cards on their paths stay bright, every other card fades, and pressing it again unfades them.
  it("desktop/agent-cards/015: Needs you only opens the path to every card that needs you and fades the rest", () => {
    cardsView();
    pressNeedsYouOnly();
    expect(needsYouOnlyChecked()).toBe(true);
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("true");
    expect(toggle(LEAD).getAttribute("aria-expanded")).toBe("true");
    expect(toggle(TRIAGE).getAttribute("aria-expanded")).toBe("true");
    const faded = (card: AgentCardEntry) => cardOf(card.key).getAttribute("data-faded") === "true";
    expect(TREE.filter(faded).map((card) => card.name)).toEqual(["coder", "unit", "solo"]);
    pressNeedsYouOnly();
    expect(TREE.filter((card) => document.querySelector(`article[data-card-key="${card.key}"]`) !== null).filter(faded)).toEqual([]);
  });

  /// Scenario: with Needs you only on, close triage's tree, then the daemon sends a list in which a new card under solo and a new card one level under the open dispatcher's unit need the user. Solo's and the unit's trees open on their own and both new cards are bright, while triage stays closed because nothing under it newly needs the user; then close the orchestrator and the coder under it comes to need the user, and the orchestrator opens again.
  it("desktop/agent-cards/015: while Needs you only is on, a card that comes to need you has the way to it opened", () => {
    const view = cardsView();
    pressNeedsYouOnly();
    fireEvent.click(toggle(TRIAGE));
    expect(toggle(TRIAGE).getAttribute("aria-expanded")).toBe("false");

    const late = entry("9", "late", { parent: SOLO.key, status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"] });
    const deep = entry("10", "deep", { parent: UNIT.key, status: "blocked", needs_you: true, needs_you_reasons: ["blocked"], relations: { dispatcher: { key: UNIT.key, name: "unit" } } });
    const solo = { ...SOLO, revision: 2, rollup: { total: 1, worst: "waiting_for_input", counts: { waiting_for_input: 1 }, needs_you: 1 } };
    const unit = { ...UNIT, revision: 2, rollup: { total: 1, worst: "blocked", counts: { blocked: 1 }, needs_you: 1 } };
    const grown = [QUEUE, LEAD, CODER, TESTER, unit, deep, solo, late, TRIAGE, STUCK];
    act(() => view.update(runtime(grown)));
    expect(toggle(SOLO).getAttribute("aria-expanded")).toBe("true");
    expect(toggle(UNIT).getAttribute("aria-expanded")).toBe("true");
    expect(cardOf(late.key).getAttribute("data-faded")).toBeNull();
    expect(cardOf(deep.key).getAttribute("data-faded")).toBeNull();
    // Nothing under triage newly needs the user: the user's close stands.
    expect(toggle(TRIAGE).getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(toggle(LEAD));
    expect(document.querySelector(`article[data-card-key="${CODER.key}"]`)).toBeNull();
    const coder = { ...CODER, revision: 2, status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"] };
    act(() => view.update(runtime([QUEUE, LEAD, coder, TESTER, unit, deep, solo, late, TRIAGE, STUCK])));
    expect(toggle(LEAD).getAttribute("aria-expanded")).toBe("true");
    expect(cardOf(CODER.key).getAttribute("data-faded")).toBeNull();
  });

  /// Scenario: open the dispatcher's tree, then render the dashboard again. The open card is remembered under `dot-agent-deck.cards.open`, keyed by deck, daemon instance and card key, and comes back open; a remembered key this daemon no longer lists is pruned; a storage that throws shows every tree closed without an error.
  it("desktop/agent-cards/016: open trees are remembered per client, pruned, and a broken storage falls back to closed", () => {
    const first = cardsView();
    fireEvent.click(toggle(QUEUE));
    const stored = JSON.parse(window.localStorage.getItem(OPEN_KEY) ?? "[]") as string[];
    expect(stored).toEqual([`${FIXTURE_DAEMON_ID}\u0000${INSTANCE}\u0000${QUEUE.key}`]);
    first.unmount();

    const gone = `${FIXTURE_DAEMON_ID}\u0000${INSTANCE}\u0000pane-99/99`;
    const otherDeck = `other-deck\u0000other-instance\u0000pane-1/1`;
    window.localStorage.setItem(OPEN_KEY, JSON.stringify([...stored, gone, otherDeck]));
    const second = cardsView();
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("true");
    expect(shownKeys()).toContain(LEAD.key);
    const pruned = JSON.parse(window.localStorage.getItem(OPEN_KEY) ?? "[]") as string[];
    expect(pruned).not.toContain(gone);
    // A deck that is not on screen keeps what it remembered.
    expect(pruned).toContain(otherDeck);
    expect(pruned).toContain(stored[0]);
    second.unmount();

    // The open cards' storage throws; the view's own key still reads Cards.
    const realGet = Storage.prototype.getItem;
    const realSet = Storage.prototype.setItem;
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(function (this: Storage, name: string) {
      if (name === OPEN_KEY) throw new Error("storage blocked");
      return realGet.call(this, name);
    });
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(function (this: Storage, name: string, value: string) {
      if (name === OPEN_KEY) throw new Error("storage blocked");
      realSet.call(this, name, value);
    });
    window.localStorage.setItem(MODE_KEY, "cards");
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    getItem.mockRestore();
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("false");
    expect(() => fireEvent.click(toggle(QUEUE))).not.toThrow();
    expect(toggle(QUEUE).getAttribute("aria-expanded")).toBe("true");
    setItem.mockRestore();
  });

  /// Scenario: put hostile or stale values under `dot-agent-deck.cards.open` and open the Cards view each time. Only the newest 500 entries are read, text longer than the app ever writes is ignored whole, malformed JSON, non-strings, overlong keys and keys of the wrong shape are dropped, a key of a replaced daemon instance is forgotten, and a deck that answers with no cards forgets its keys while a deck that is not answering keeps them.
  it("desktop/agent-cards/016: what is read back is bounded and checked, and an answering deck with no cards is pruned", () => {
    const queue = placed(QUEUE);
    const others = Array.from({ length: 500 }, (_, at) => `other-deck\u0000other-instance\u0000pane-${at}/${at}`);
    const shownOpen = (stored: string, entries: AgentCardEntry[] = TREE) => {
      window.localStorage.setItem(OPEN_KEY, stored);
      const view = cardsView(runtime(entries));
      const open = entries.includes(QUEUE) ? toggle(QUEUE).getAttribute("aria-expanded") === "true" : false;
      return { open, view };
    };
    const storedNow = () => JSON.parse(window.localStorage.getItem(OPEN_KEY) ?? "[]") as unknown[];

    // Only the newest 500 are read: the oldest goes.
    let run = shownOpen(JSON.stringify([queue, ...others]));
    expect(run.open, "the 501st-newest key is not read").toBe(false);
    run.view.unmount();
    run = shownOpen(JSON.stringify([...others, queue]));
    expect(run.open, "the newest key is read").toBe(true);
    run.view.unmount();

    // Stored text longer than the app ever writes is ignored whole.
    run = shownOpen(JSON.stringify([queue, "x".repeat(2_000_000)]));
    expect(run.open).toBe(false);
    run.view.unmount();

    // Malformed JSON reads as nothing open, without an error.
    run = shownOpen("[not json");
    expect(run.open).toBe(false);
    run.view.unmount();

    // Non-strings, an overlong key and keys of the wrong shape are dropped; the valid key stays.
    const overlong = `other-deck\u0000other-instance\u0000${"p".repeat(600)}`;
    run = shownOpen(JSON.stringify([42, { key: queue }, null, overlong, "no-separators", "other-deck\u0000x", queue]));
    expect(run.open).toBe(true);
    fireEvent.click(toggle(TRIAGE));
    expect(storedNow()).toEqual([queue, placed(TRIAGE)]);
    run.view.unmount();

    // A replaced daemon instance's keys are forgotten.
    window.localStorage.clear();
    run = shownOpen(JSON.stringify([placed(QUEUE, "old-instance"), queue]));
    expect(run.open).toBe(true);
    expect(storedNow()).toEqual([queue]);
    run.view.unmount();

    // A deck that answers with no cards forgets its keys; another deck's stay.
    window.localStorage.clear();
    const otherDeck = others[0];
    run = shownOpen(JSON.stringify([queue, otherDeck]), []);
    expect(storedNow()).toEqual([otherDeck]);
    run.view.unmount();
  });

  /// Scenario: open the Cards view on a daemon older than the app, which does not advertise the tree. Every card it lists is a root in its order, none has a toggle or a roll-up, and one line says the deck's daemon is older than the app.
  it("desktop/agent-cards/017: an older daemon's cards are all roots, with no roll-ups, and a note says why", () => {
    cardsView(runtime(TREE, { tree: false }));
    expect(roots()).toEqual(TREE.map((card) => card.key));
    expect(screen.queryAllByTestId("agent-card-toggle")).toHaveLength(0);
    expect(screen.queryAllByTestId("agent-card-rollup")).toHaveLength(0);
    expect(screen.queryAllByTestId("agent-card-ref")).toHaveLength(0);
    expect(screen.getByTestId("agent-cards-older-daemon").textContent).toMatch(/older than (this|the) app/);
  });

  /// Scenario: open the Cards view while the deck has no cards, then the daemon lists some. The grid is laid out for the frame's width as soon as the cards appear, and again when the frame is resized; the dashboard filter then hides every card and is cleared, and the grid is measured afresh for the new width and keeps following resizes.
  it("desktop/agent-cards/020: the grid measures its frame whenever cards appear, including after a filter emptied it", () => {
    let frameWidth = 900;
    const observers: { callback: () => void; targets: Element[]; disconnected: boolean }[] = [];
    const realObserver = globalThis.ResizeObserver;
    globalThis.ResizeObserver = class {
      private readonly record: { callback: () => void; targets: Element[]; disconnected: boolean };
      constructor(callback: () => void) {
        this.record = { callback, targets: [], disconnected: false };
        observers.push(this.record);
      }
      observe(target: Element) { this.record.targets.push(target); }
      unobserve() { /* not used by the grid */ }
      disconnect() { this.record.disconnected = true; }
    } as unknown as typeof ResizeObserver;
    const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, "clientWidth");
    Object.defineProperty(HTMLElement.prototype, "clientWidth", {
      configurable: true,
      get(this: HTMLElement) { return this.classList.contains("agent-cards-deck") ? frameWidth : 0; },
    });
    try {
      const columns = () => (screen.getAllByTestId("agent-cards-roots")[0] as HTMLElement).style.gridTemplateColumns;
      const resize = (next: number) => {
        frameWidth = next;
        const frame = document.querySelector(".agent-cards-deck");
        const live = observers.filter((observer) => !observer.disconnected && observer.targets.includes(frame as Element));
        expect(live.length, "the frame on screen is watched").toBeGreaterThan(0);
        act(() => live.forEach((observer) => observer.callback()));
      };

      const view = cardsView(runtime([]));
      expect(document.querySelector(".agent-cards-deck")).toBeNull();
      act(() => view.update(runtime(TREE)));
      expect(columns()).toBe("repeat(3, minmax(0px, 224px))");
      resize(500);
      expect(columns()).toBe("repeat(2, minmax(0px, 224px))");

      act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "zz-matches-no-agent" }));
      expect(document.querySelector(".agent-cards-deck")).toBeNull();
      frameWidth = 1200;
      act(() => setDashboardFilter(clearDashboardFilter()));
      expect(columns()).toBe("repeat(5, minmax(0px, 224px))");
      resize(300);
      expect(columns()).toBe("repeat(1, minmax(0px, 224px))");
    } finally {
      globalThis.ResizeObserver = realObserver;
      if (descriptor) Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get: descriptor.get });
      else delete (HTMLElement.prototype as unknown as Record<string, unknown>).clientWidth;
    }
  });

  /// Scenario: the daemon sends each card's kind. A dispatcher whose only unit the dashboard filter hides still reads "dispatcher", as does one the daemon names a dispatcher with no unit listed at all; against an older daemon, which sends no kind, the app reads the kind off the cards on screen instead.
  it("desktop/agent-cards/021: a card's kind is the daemon's, whatever the dashboard filter shows", () => {
    const boss = entry("1", "boss-dispatcher", { kind: "dispatcher", rollup: { total: 1, worst: "working", counts: { working: 1 }, needs_you: 0 } });
    const worker = entry("2", "worker-unit", { kind: "agent", parent: boss.key, status: "working", relations: { dispatcher: { key: boss.key, name: "boss-dispatcher" } } });
    const lone = entry("3", "lone-dispatcher", { kind: "dispatcher" });
    const kindOf = (card: AgentCardEntry) => within(cardOf(card.key)).getByTestId("agent-card-kind").textContent;
    const view = cardsView(runtime([boss, worker, lone]));
    expect(kindOf(boss)).toBe("dispatcher");
    expect(kindOf(lone)).toBe("dispatcher");
    act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "dispatcher" }));
    expect(document.querySelector(`article[data-card-key="${worker.key}"]`), "the filter hides the unit").toBeNull();
    expect(kindOf(boss)).toBe("dispatcher");
    expect(kindOf(lone)).toBe("dispatcher");
    act(() => setDashboardFilter(clearDashboardFilter()));

    // An older daemon sends no kind (the app drops what it did not vouch for): the relations on screen decide.
    act(() => view.update(runtime([boss, worker, lone], { tree: false })));
    expect(kindOf(boss)).toBe("dispatcher");
    expect(kindOf(lone)).toBe("agent");
  });

  /// Scenario: a malformed list links two cards to each other, both remembered open, one needing the user. The view draws each card once, the first as the root and the other under it, and Needs you only works on it without looping.
  it("desktop/agent-cards/022: a cyclic list is drawn once, with persisted open cards and Needs you only", () => {
    const first = entry("1", "first-loop", { parent: key("2"), status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"], rollup: { total: 1, worst: "idle", counts: { idle: 1 }, needs_you: 0 } });
    const second = entry("2", "second-loop", { parent: key("1"), rollup: { total: 1, worst: "waiting_for_input", counts: { waiting_for_input: 1 }, needs_you: 1 } });
    window.localStorage.setItem(OPEN_KEY, JSON.stringify([placed(first), placed(second)]));
    cardsView(runtime([first, second]));
    expect(roots()).toEqual([first.key]);
    expect(shownKeys()).toEqual([first.key, second.key]);
    pressNeedsYouOnly();
    expect(shownKeys()).toEqual([first.key, second.key]);
    expect(cardOf(first.key).getAttribute("data-faded")).toBeNull();
    fireEvent.click(toggle(first));
    fireEvent.click(toggle(first));
    expect(shownKeys()).toEqual([first.key, second.key]);
  });

  /// Scenario: ask which addresses an issue badge may open. Only a github.com issue's own page by its number, with at most a trailing slash, opens; a page under it, a query, a fragment, number 0 or a leading zero, another site, plain http, a login or another port does not; an explicit :443 is the same address and does.
  it("desktop/agent-cards/023: an issue badge opens only an issue's own numbered page", () => {
    for (const good of [
      "https://github.com/o/r/issues/1",
      "https://github.com/vfarcic/dot-agent-deck/issues/2418",
      "https://github.com/o/r/issues/3/",
      "https://github.com:443/o/r/issues/3",
    ]) {
      expect(isIssueUrl(good), good).toBe(true);
      expect(referenceUrl({ url: good }), good).toBe(good);
    }
    for (const bad of [
      "https://github.com/o/r/issues/1/anything",
      "https://github.com/o/r/issues/1?return_to=https://example.com",
      "https://github.com/o/r/issues/1?",
      "https://github.com/o/r/issues/1#issuecomment-1",
      "https://github.com/o/r/issues/0",
      "https://github.com/o/r/issues/01",
      "https://github.com/o/r/issues",
      "https://github.com/o/r/issues/abc",
      "https://github.com/o/r/pull/1",
      "https://github.com.evil.example/o/r/issues/1",
      "http://github.com/o/r/issues/1",
      "https://user:pass@github.com/o/r/issues/1",
      "https://github.com:8443/o/r/issues/1",
    ]) {
      expect(isIssueUrl(bad), bad).toBe(false);
    }
  });

  /// Scenario: open the unit's card, whose agent reported six pull requests and issues. Its agent's Card tab lists every one in the daemon's order with the deck's check (state, review and CI, or the issue's state, and the title), how fresh that check is or why there is none, and what the agent wrote; a checked badge there opens its page as the card's does, and one the deck could not check has no link.
  it("desktop/agent-cards/025: the Card tab lists every checked reference with its state, its notes and the agent's claim, and opens them", () => {
    cardsView();
    fireEvent.click(toggle(QUEUE));
    fireEvent.click(within(cardOf(UNIT.key)).getByTestId("agent-card-details"));
    const section = within(screen.getByTestId("agent-card-panel")).getByRole("region", { name: "Pull requests and issues" });
    const items = within(section).getAllByTestId("agent-card-panel-ref");
    expect(items.map((item) => item.getAttribute("data-field"))).toEqual(["pull-request", "issue", "pull-request", "pr-two", "issue-two", "pr-three"]);
    expect(items.map((item) => item.getAttribute("data-ref-status"))).toEqual(["checked", "checked", "pending", "unavailable", "unavailable", "stale"]);
    const [merged, closed, pending, notFound, unrecognised, stale] = items;
    const text = (item: HTMLElement, id: string) => within(item).getByTestId(id).textContent ?? "";

    // The deck's check: a pull request's state, review and CI; an issue's state; the title; when it was checked.
    expect(text(merged, "agent-card-panel-ref-checked")).toBe("Pull request #24180: merged, approved; CI: 1 failing, 3 passing");
    expect(merged.textContent).toContain("Cards, first slice");
    expect(text(merged, "agent-card-panel-ref-note")).toMatch(/^Checked by the deck /);
    expect(text(merged, "agent-card-panel-ref-claim")).toContain("#24180");
    expect(within(merged).getByText("agent says")).toBeInTheDocument();
    expect(text(closed, "agent-card-panel-ref-checked")).toBe("Issue #3: closed");
    expect(text(closed, "agent-card-panel-ref-claim")).toContain("o/r#3");
    for (const label of ISSUE_LABELS) expect(within(closed).getByText(label.name)).toBeVisible();

    // Still to be checked, not found (with the daemon's detail), not a GitHub reference: said, and no link.
    expect(text(pending, "agent-card-panel-ref-note")).toMatch(/not looked #7 up yet/);
    expect(text(notFound, "agent-card-panel-ref-note")).toContain("it may be private");
    expect(text(notFound, "agent-card-panel-ref-note")).toContain("Could not resolve to a PullRequest");
    expect(notFound.textContent).toContain("not found or private");
    expect(text(unrecognised, "agent-card-panel-ref-note")).toContain("it is not a GitHub pull request or issue");
    expect(text(unrecognised, "agent-card-panel-ref-claim")).toContain("see the notes");
    for (const item of [pending, notFound, unrecognised]) expect(within(item).queryByRole("button")).toBeNull();

    // Stale: the last state the deck saw, marked stale, and when that was.
    expect(within(stale).getByTestId("agent-card-panel-pr").getAttribute("data-pr-state")).toBe("open");
    expect(stale.textContent).toContain("stale");
    expect(text(stale, "agent-card-panel-ref-note")).toMatch(/^Last checked .*may be out of date\.$/);

    // A link the shell refuses says why in the Card tab.
    openReference.mockReturnValueOnce("That card is no longer on its daemon, so its page was not opened.");
    act(() => { fireEvent.click(within(stale).getByTestId("agent-card-panel-pr")); });
    expect(openReference).toHaveBeenLastCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, cardKey: UNIT.key, kind: "pull_request", number: 9, url: PR_STALE.url });
    expect(within(screen.getByTestId("agent-card-panel")).getByTestId("agent-card-panel-notice").textContent).toBe("That card is no longer on its daemon, so its page was not opened.");

    // The links open through the same path as the card's badges, named by the card; the page opens
    // over the pane, and an open that happened clears the refusal.
    act(() => { fireEvent.click(within(closed).getByTestId("agent-card-panel-issue")); });
    expect(openReference).toHaveBeenLastCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, cardKey: UNIT.key, kind: "issue", number: 3, url: ISSUE_CLOSED.url });
    expect(screen.queryByTestId("agent-card-panel-notice"), "an open that happened clears the refusal").toBeNull();
    expect(screen.queryByTestId("overview-card-notice")).toBeNull();
  });

  /// Scenario: press one of the unit's reference badges on its card while the shell refuses to open it. The dashboard says why in a banner, which Dismiss clears, instead of doing nothing.
  it("desktop/agent-cards/025: a reference the shell refuses to open says why on the dashboard", () => {
    cardsView();
    fireEvent.click(toggle(QUEUE));
    openReference.mockReturnValueOnce("That card's daemon is not answering, so its page was not opened.");
    act(() => { fireEvent.click(within(cardOf(UNIT.key)).getByTestId("agent-card-issue")); });
    const banner = screen.getByTestId("overview-card-notice");
    expect(banner.textContent).toContain("That card's daemon is not answering, so its page was not opened.");
    act(() => { fireEvent.click(within(banner).getByRole("button", { name: "Dismiss" })); });
    expect(screen.queryByTestId("overview-card-notice")).toBeNull();
  });

  /// Scenario: lay out the unit's card with a reference line that holds two of its six badges. The two show, the other four are hidden whole and a "+4" button says so; pressing it opens the unit's pane with its Card tab at its list of every reference.
  it("desktop/agent-cards/025: references that do not fit are hidden whole and counted by a +N that opens the Card tab at them", () => {
    // jsdom lays nothing out: give the line 200 px and each badge 60 px with a 6 px gap.
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.classList.contains("agent-card-refs") ? 200 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.classList.contains("agent-card-ref") ? 60 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function left(this: HTMLElement) { return this.classList.contains("agent-card-ref") ? [...(this.parentElement?.children ?? [])].indexOf(this) * 66 : 0; });
    cardsView();
    fireEvent.click(toggle(QUEUE));
    const unit = cardOf(UNIT.key);
    const refs = within(unit).getAllByTestId("agent-card-ref");
    expect(refs).toHaveLength(6);
    // Three fit the bare line; with room kept for the count, two do.
    expect(refs.map((node) => node.getAttribute("data-overflow"))).toEqual([null, null, "true", "true", "true", "true"]);
    for (const hidden of refs.slice(2)) expect(hidden.getAttribute("aria-hidden")).toBe("true");
    const more = within(unit).getByTestId("agent-card-refs-more");
    expect(more.textContent).toBe("+4");
    expect(more.getAttribute("aria-label")).toBe("4 more: Show all 6 pull requests and issues on unit's card");
    const scrolled = vi.fn();
    Element.prototype.scrollIntoView = scrolled;
    act(() => { fireEvent.click(more); });
    expect(screen.getByTestId("test-agent-pane").getAttribute("data-card-section")).toBe("references");
    const panel = screen.getByTestId("agent-card-panel");
    expect(panel.getAttribute("data-card-key")).toBe(UNIT.key);
    const section = within(panel).getByTestId("agent-card-panel-refs");
    expect(scrolled.mock.contexts).toContain(section);
    expect(within(section).getAllByTestId("agent-card-panel-ref")).toHaveLength(6);
    delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView;
  });
});


/*
 * Issue #2533 (PRD #2374 decision 2026-11-26) — an opened tree wraps into rows
 * instead of scrolling sideways, in a box of its own captioned with its root.
 * jsdom lays nothing out, so the frame's width is set and what is asserted is
 * what the layout drew for it: the rows, their buses and spines, the boxes.
 */
describe("desktop/agent-cards/144 — an opened tree wraps into rows, in a captioned box (issue #2533)", () => {
  const DISPATCHER = entry("20", "dispatcher", { rollup: { total: 11, worst: "working", counts: { working: 11 }, needs_you: 0 } });
  const units = Array.from({ length: 7 }, (_, at) => entry(`2${at + 1}`, `unit-${at + 1}`, { parent: key("20"), status: "working", relations: { dispatcher: { key: key("20"), name: "dispatcher" } } }));
  const TEAM = entry("30", "team", { parent: key("20"), status: "working", relations: { dispatcher: { key: key("20"), name: "dispatcher" }, orchestration: { id: "o-9", label: "team", role: "lead", is_orchestrator: true } }, rollup: { total: 4, worst: "working", counts: { working: 4 }, needs_you: 0 } });
  const roles = Array.from({ length: 4 }, (_, at) => entry(`3${at + 1}`, `role-${at + 1}`, { parent: key("30"), status: "working", relations: { orchestration: { id: "o-9", label: "team", role: `role-${at + 1}`, is_orchestrator: false } } }));
  const PAIR = entry("40", "pair", { rollup: { total: 2, worst: "idle", counts: { idle: 2 }, needs_you: 0 } });
  const pair = [entry("41", "left", { parent: key("40") }), entry("42", "right", { parent: key("40") })];
  const WIDE = [DISPATCHER, ...units, TEAM, ...roles, PAIR, ...pair];

  let frameWidth = 1000;
  let descriptor: PropertyDescriptor | undefined;
  beforeEach(() => {
    descriptor = Object.getOwnPropertyDescriptor(Element.prototype, "clientWidth");
    Object.defineProperty(HTMLElement.prototype, "clientWidth", {
      configurable: true,
      get(this: HTMLElement) { return this.classList.contains("agent-cards-deck") ? frameWidth : 0; },
    });
  });
  afterEach(() => {
    if (descriptor) Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get: descriptor.get });
    else delete (HTMLElement.prototype as unknown as Record<string, unknown>).clientWidth;
  });

  const boxOf = (root: AgentCardEntry) => {
    const band = document.querySelector(`[data-testid='agent-cards-band'][data-root-key="${root.key}"]`);
    if (!(band instanceof HTMLElement)) throw new Error(`no tree open under ${root.name}`);
    return within(band).getByTestId("agent-cards-box");
  };
  /** A level's rows, each as the names of the cards in it. */
  const itemsByRow = (level: Element) => {
    const rows: Element[][] = [];
    for (const item of level.querySelectorAll(":scope > .agent-cards-tree-item")) (rows[Number(item.getAttribute("data-row"))] ??= []).push(item);
    return rows;
  };
  const rowsOf = (level: Element) => itemsByRow(level)
    .map((row) => row.map((item) => within(item.querySelector(":scope > article") as HTMLElement).getByTestId("agent-card-open-agent").textContent));
  const topLevel = (root: AgentCardEntry) => boxOf(root).querySelector(":scope > [data-testid='agent-cards-tree-level']")!;
  const lines = (level: Element) => itemsByRow(level).map((row) => ({
    bus: row.flatMap((item) => [...item.querySelectorAll(":scope > [data-testid='agent-cards-bus']")]).length,
    spine: row.flatMap((item) => [...item.querySelectorAll(":scope > [data-testid='agent-cards-spine']")]).length,
  }));

  /// Scenario: at a frame 1300 pixels wide, open a dispatcher with eight agents under it and a root with two beside it. Each tree is in its own box named after its root; the dispatcher's eight wrap into two rows, five and three, each row with its own line and one spine joining the first row to the second; the pair fits in one row and has no spine.
  it("desktop/agent-cards/144: each opened root's tree is in its own captioned box, wrapped into rows each joined to the parent", () => {
    frameWidth = 1300;
    cardsView(runtime(WIDE));
    fireEvent.click(toggle(DISPATCHER));
    fireEvent.click(toggle(PAIR));
    expect(screen.getAllByTestId("agent-cards-box")).toHaveLength(2);
    expect(within(boxOf(DISPATCHER)).getByTestId("agent-cards-box-caption").textContent).toBe("dispatcher");
    expect(within(boxOf(PAIR)).getByTestId("agent-cards-box-caption").textContent).toBe("pair");
    expect(boxOf(DISPATCHER).getAttribute("aria-label")).toBe("Agents under dispatcher");
    expect(boxOf(PAIR).getAttribute("role")).toBe("group");
    // Every box has the same look: no per-tree colour, only the class the theme's tint comes from.
    for (const box of screen.getAllByTestId("agent-cards-box")) {
      expect(box.className).toBe("agent-cards-box");
      expect(box.style.background).toBe("");
      expect(box.style.backgroundColor).toBe("");
    }

    expect(rowsOf(topLevel(DISPATCHER))).toEqual([
      ["unit-1", "unit-2", "unit-3", "unit-4", "unit-5"],
      ["unit-6", "unit-7", "team"],
    ]);
    expect(lines(topLevel(DISPATCHER))).toEqual([{ bus: 1, spine: 1 }, { bus: 1, spine: 0 }]);
    expect(rowsOf(topLevel(PAIR))).toEqual([["left", "right"]]);
    expect(lines(topLevel(PAIR))).toEqual([{ bus: 1, spine: 0 }]);
    // The roots stay where they were, side by side in the first row, in the daemon's order.
    expect(roots()).toEqual([DISPATCHER.key, PAIR.key]);
  });

  /// Scenario: with the dispatcher's tree open, narrow the frame from 1300 to 760 and then to 420 pixels. Its eight agents rewrap into more rows each time, every row but the last with a spine to the next; opening the orchestration inside it shows its four roles as a level of their own under it, wrapped to fit too.
  it("desktop/agent-cards/144: a narrower frame wraps into more rows, at every level", () => {
    const observers: (() => void)[] = [];
    const realObserver = globalThis.ResizeObserver;
    globalThis.ResizeObserver = class {
      constructor(callback: () => void) { observers.push(callback); }
      observe() { /* every callback is run on a resize */ }
      unobserve() { /* not used by the grid */ }
      disconnect() { /* not used here */ }
    } as unknown as typeof ResizeObserver;
    try {
      frameWidth = 1300;
      cardsView(runtime(WIDE));
      fireEvent.click(toggle(DISPATCHER));
      const resize = (next: number) => { frameWidth = next; act(() => observers.forEach((callback) => callback())); };
      const counts = () => rowsOf(topLevel(DISPATCHER)).map((row) => row.length);

      expect(counts()).toEqual([5, 3]);
      resize(760);
      expect(counts()).toEqual([3, 3, 2]);
      expect(lines(topLevel(DISPATCHER)).map((row) => row.spine)).toEqual([1, 1, 0]);
      resize(420);
      expect(counts()).toEqual([1, 1, 1, 1, 1, 1, 1, 1]);
      expect(lines(topLevel(DISPATCHER)).map((row) => row.spine)).toEqual([1, 1, 1, 1, 1, 1, 1, 0]);

      resize(760);
      fireEvent.click(toggle(TEAM));
      const teamCard = cardOf(TEAM.key);
      const nested = teamCard.parentElement!.querySelector(":scope > [data-testid='agent-cards-tree-level']");
      expect(nested, "the orchestration's roles are a level under its card").not.toBeNull();
      expect(boxOf(DISPATCHER).contains(nested)).toBe(true);
      expect(rowsOf(nested!).flat()).toEqual(["role-1", "role-2", "role-3", "role-4"]);
      expect(rowsOf(nested!).length, "the roles wrap within the room left under the orchestration").toBeGreaterThan(1);
      expect(cardOf(roles[0].key).getAttribute("data-depth")).toBe("2");
      // The open orchestration takes a row of its own, and the spine passes it.
      expect(rowsOf(topLevel(DISPATCHER)).find((row) => row.includes("team"))).toEqual(["team"]);
      expect(screen.getAllByTestId("agent-cards-box")).toHaveLength(1);
    } finally {
      globalThis.ResizeObserver = realObserver;
    }
  });

  /// Scenario: at a 1300 pixel frame, open the dispatcher, put the keyboard on its orchestration's open arrow, which shares a row with two other agents, and press it. The orchestration's roles open under it and it moves to a row of its own, and the keyboard is still on its arrow (PR #2550 review).
  it("desktop/agent-cards/144: opening a card that moves to another row keeps the keyboard on it", () => {
    frameWidth = 1300;
    cardsView(runtime(WIDE));
    fireEvent.click(toggle(DISPATCHER));
    expect(rowsOf(topLevel(DISPATCHER))[1]).toEqual(["unit-6", "unit-7", "team"]);
    const arrow = toggle(TEAM);
    arrow.focus();
    fireEvent.click(arrow, { detail: 0 });
    expect(rowsOf(topLevel(DISPATCHER)).find((row) => row.includes("team"))).toEqual(["team"]);
    expect(toggle(TEAM).getAttribute("aria-expanded")).toBe("true");
    expect(toggle(TEAM), "the same arrow is still on screen").toBe(arrow);
    expect(document.activeElement).toBe(arrow);
  });
});
