import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { OVERVIEW_MODE_STORAGE_KEY } from "../lib/agentCards";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { deckName, domIdentity } from "../lib/displayText";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardEntry, type AgentCardsSnapshot, type AgentSession, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";
import { NO_UNCOUNTED, needsYouCountDescription, uncountedDaemonsText } from "./AgentCardsGrid";
import { cardsViewShown, dashboardMore, needsYouOnlyChecked, pressNeedsYouOnly } from "../test/needsYouMenu";

/*
 * PR #2491 decision 2026-11-25 — the Dashboard kept uncluttered: a ⋯ menu per
 * daemon (Card actions), a global ⋯ menu in the top bar (Needs you only, which
 * now works in the Table view too), a chip while Needs you only is on, and no
 * Cards toolbar row (`desktop/agent-cards/111`–`113`).
 */

const INSTANCE = "daemon-instance-more";
const REMOTE_INSTANCE = "daemon-instance-more-remote";
const key = (id: string) => `pane-${id}/${id}`;

function entry(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: key(agentId), instance_id: INSTANCE, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "working", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [], primary_action_id: "",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-25T00:00:00Z", ...overrides,
  };
}

/*
 * lead ── coder (needs you)
 *     └── tester
 * lone
 * boss ── mid ── deep (needs you)
 */
const LEAD = entry("1", "lead");
const CODER = entry("2", "coder", { parent: key("1"), status: "waiting_for_input", needs_you: true, needs_you_reasons: ["question"] });
const TESTER = entry("3", "tester", { parent: key("1") });
const LONE = entry("4", "lone");
const BOSS = entry("5", "boss");
const MID = entry("6", "mid", { parent: key("5") });
const DEEP = entry("7", "deep", { parent: key("6"), status: "waiting_for_input", needs_you: true, needs_you_reasons: ["question"] });
const TREE = [LEAD, CODER, TESTER, LONE, BOSS, MID, DEEP];

function snapshot(entries: AgentCardEntry[], extra: Partial<AgentCardsSnapshot> = {}): AgentCardsSnapshot {
  return { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries, catalogErrors: [], tree: true, ...extra };
}

/*
 * `cards: false` on either daemon leaves it out of `agentCards`: a daemon
 * that does not report cards, whose rows Needs you only leaves as they are.
 * `status` sets each of its agents' status, by agent id.
 */
function runtime({ entries = TREE, remote, catalogEdit = true, cards = true, status = {} }: { entries?: AgentCardEntry[]; remote?: { entries: AgentCardEntry[]; catalogEdit: boolean; cards?: boolean }; catalogEdit?: boolean; cards?: boolean; status?: Record<string, AgentSession["status"]> } = {}): DeckRuntimeState {
  const [local, far] = createFixtureFleet("fleet");
  const agentsOf = (list: AgentCardEntry[], daemonId: string, incarnation: string) => list.map((card) => ({ ...createFixtureStartedAgent({ id: card.agent_id, daemonId, displayName: card.name, command: "claude", cwd: "/w" }), incarnation, ...(status[card.agent_id] ? { status: status[card.agent_id] } : {}) }));
  const fleet = [{ ...local, connection: { ...local.connection, incarnation: INSTANCE }, agents: agentsOf(entries, FIXTURE_DAEMON_ID, INSTANCE) }];
  const agentCards: Record<string, AgentCardsSnapshot> = cards ? { [FIXTURE_DAEMON_ID]: snapshot(entries, { catalogEdit }) } : {};
  if (remote) {
    fleet.push({ ...far, connection: { ...far.connection, incarnation: REMOTE_INSTANCE }, agents: agentsOf(remote.entries, FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE) });
    if (remote.cards !== false) agentCards[FIXTURE_REMOTE_DAEMON_ID] = { deckId: FIXTURE_REMOTE_DAEMON_ID, instanceId: REMOTE_INSTANCE, revision: 1, entries: remote.entries.map((card) => ({ ...card, instance_id: REMOTE_INSTANCE })), catalogErrors: [], tree: true, catalogEdit: remote.catalogEdit };
  }
  return {
    mode: "fixture", snapshot: fleet[0], fleet, terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards, invokeCardAction: vi.fn(), invokeCardActionBatch: vi.fn(),
    readCardCatalog: vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" })),
    editCardCatalog: vi.fn(),
  } as unknown as DeckRuntimeState;
}

/** The Table's rows on screen, by agent name, in order. */
const tableRows = () => screen.queryAllByRole("row", { name: /^Open .* agent$/ }).map((row) => row.getAttribute("aria-label")!.replace(/^Open (.*) agent$/, "$1"));
const rowOf = (daemonId: string, agentId: string) => screen.queryByTestId(`overview-agent-${domIdentity(agentKey({ daemonId, id: agentId }))}`);
const daemonSection = (deckId: string) => screen.getAllByTestId("daemon-group").find((node) => node.getAttribute("data-daemon-id") === domIdentity(deckId))!;

beforeEach(() => {
  window.localStorage.clear();
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => {
  cleanup();
  window.localStorage.clear();
  act(() => setDashboardFilter(clearDashboardFilter()));
});

describe("the Dashboard's ⋯ menus (PR #2491 decision 2026-11-25)", () => {
  /// Scenario: On the Cards view, look at the top bar and above the cards, then open the top bar's ⋯ menu and press Needs you only. There is no line above the cards and no Needs you only or Card actions button outside a menu; the ⋯ right of Refresh is named "More", counts the two agents that need you, and opens a menu holding Needs you only as a checkable item; pressing it ticks it, fades the cards that do not need you and shows a "Needs you only" chip beside Filter, and pressing the chip turns it off in one click and the chip goes.
  it("desktop/agent-cards/111: the top bar's ⋯ holds Needs you only, the chip turns it off, and the Cards toolbar row is gone", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(cardsViewShown()).toBe(true);
    expect(document.querySelector(".agent-cards-toolbar")).toBeNull();
    expect(screen.queryByTestId("agent-cards-needs-you-filter")).toBeNull();
    expect(screen.queryByTestId("agent-cards-actions-editor-open")).toBeNull();
    expect(screen.queryByRole("button", { name: "Needs you only" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Card actions" })).toBeNull();

    const more = dashboardMore();
    expect(more).toHaveAttribute("title", "More");
    expect(more).toHaveAttribute("aria-haspopup", "menu");
    expect(more).toHaveAttribute("aria-expanded", "false");
    expect(more.closest(".more-menu")!.previousElementSibling, "right of Refresh").toBe(screen.getByTestId("overview-refresh"));
    expect(screen.getByTestId("overview-needs-you-count").textContent).toBe("2");
    expect(more).toHaveAccessibleDescription("2 agents need you");

    expect(screen.queryByTestId("needs-you-chip")).toBeNull();
    pressNeedsYouOnly();
    expect(needsYouOnlyChecked()).toBe(true);
    const chip = screen.getByTestId("needs-you-chip");
    expect(chip).toHaveTextContent("Needs you only");
    expect(chip).toHaveAccessibleName("Needs you only: show every agent");
    expect(chip.closest(".top-actions")).not.toBeNull();
    const faded = [...document.querySelectorAll("article[data-faded='true']")].map((node) => node.getAttribute("aria-label"));
    expect(faded).toEqual(expect.arrayContaining(["tester", "lone"]));

    fireEvent.click(chip);
    expect(screen.queryByTestId("needs-you-chip")).toBeNull();
    expect(needsYouOnlyChecked()).toBe(false);
    expect(document.querySelectorAll("article[data-faded='true']")).toHaveLength(0);
  });

  /// Scenario: On the Table view, turn Needs you only on from the top bar's ⋯ menu. The view stays the Table; it shows coder and deep, which need you, with lead above coder and boss and mid above deep, and hides tester and lone; no row is faded. Turned off with the chip, every row is back.
  it("desktop/agent-cards/112: Needs you only in the Table keeps the rows that need you and the rows above them", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
    const all = ["lead", "coder", "tester", "lone", "boss", "mid", "deep"];
    expect([...tableRows()].sort()).toEqual([...all].sort());

    pressNeedsYouOnly();
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked"), "the view stays the Table").toBe("true");
    expect(cardsViewShown()).toBe(false);
    expect([...tableRows()].sort()).toEqual(["boss", "coder", "deep", "lead", "mid"]);
    expect(rowOf(FIXTURE_DAEMON_ID, TESTER.agent_id)).toBeNull();
    expect(rowOf(FIXTURE_DAEMON_ID, LONE.agent_id)).toBeNull();
    expect(document.querySelectorAll("tr[data-faded], tr.is-faded")).toHaveLength(0);
    expect(screen.getByTestId("needs-you-chip")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("needs-you-chip"));
    expect([...tableRows()].sort()).toEqual([...all].sort());
  });

  /// Scenario: On the Table view with Needs you only on, the daemon's cards stop marking anyone as needing you. The daemon's section says in one line that no agent needs you, the dashboard says nothing needs you right now, and the ⋯ carries no count.
  it("desktop/agent-cards/112: with Needs you only on and nobody needing you, the Table says so", () => {
    const view = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    pressNeedsYouOnly();
    const calm = TREE.map((card) => ({ ...card, needs_you: false, needs_you_reasons: [], status: "working" }));
    view.rerender(<AgentOverview runtime={runtime({ entries: calm })} onNavigate={vi.fn()} />);
    expect(tableRows()).toEqual([]);
    expect(screen.getByTestId("overview-needs-you-empty")).toHaveTextContent(/no agent needs you/);
    expect(screen.getByTestId("agent-cards-none-need-you")).toHaveTextContent("Nothing needs you right now.");
    expect(screen.queryByTestId("overview-needs-you-count")).toBeNull();
    expect(dashboardMore()).not.toHaveAttribute("aria-describedby");
  });

  /// Scenario: On the Table view, the only daemon reports no cards, and one of its agents needs input. Turn Needs you only on from the top bar's ⋯. Every row stays, the agent that needs input among them; the dashboard does not say nothing needs you, but that no card needs you and that the daemon's agents are shown as usual because it does not report cards. The ⋯ carries no count, and its description says the daemon's agents are not counted.
  it("desktop/agent-cards/112: with no daemon reporting cards, Needs you only does not say nothing needs you", () => {
    const calm = TREE.map((card) => ({ ...card, needs_you: false, needs_you_reasons: [] }));
    render(<AgentOverview runtime={runtime({ entries: calm, cards: false, status: { [CODER.agent_id]: "needs_input" } })} onNavigate={vi.fn()} />);
    const name = deckName(createFixtureFleet("fleet")[0].connection);
    pressNeedsYouOnly();
    expect([...tableRows()].sort(), "a daemon without cards keeps its rows").toEqual(TREE.map((card) => card.name).sort());
    expect(screen.queryByTestId("agent-cards-none-need-you")).toBeNull();
    expect(document.body).not.toHaveTextContent("Nothing needs you");
    const note = screen.getByTestId("agent-cards-none-need-you-scoped");
    expect(note).toHaveTextContent(`No card needs you. Agents on ${name} are shown as usual: that daemon does not report cards.`);
    expect(screen.queryByTestId("overview-needs-you-count")).toBeNull();
    expect(dashboardMore()).toHaveAccessibleDescription(`No card needs you. Agents on ${name} are not counted: that daemon does not report cards.`);
  });

  /// Scenario: On the Table view, two daemons: one reports cards and none of them needs you, the other reports none and one of its agents needs input. Turn Needs you only on from the top bar's ⋯. The quiet card daemon says no agent needs you, the other keeps its rows; the dashboard names the daemon without cards rather than saying nothing needs you. With a card that needs you, the ⋯ counts it and its description says the other daemon's agents are not counted.
  it("desktop/agent-cards/112: in a mixed fleet, a quiet card daemon does not make the dashboard say nothing needs you", () => {
    const calm = TREE.map((card) => ({ ...card, needs_you: false, needs_you_reasons: [], status: "working" }));
    const legacy = [entry("11", "legacy-lead"), entry("12", "legacy-asker")];
    const view = render(<AgentOverview runtime={runtime({ entries: calm, remote: { entries: legacy, catalogEdit: false, cards: false }, status: { "12": "needs_input" } })} onNavigate={vi.fn()} />);
    const name = deckName(createFixtureFleet("fleet")[1].connection);
    pressNeedsYouOnly();
    expect(within(daemonSection(FIXTURE_DAEMON_ID)).getByTestId("overview-needs-you-empty")).toHaveTextContent(/no agent needs you/);
    expect([...tableRows()].sort()).toEqual(["legacy-asker", "legacy-lead"]);
    expect(screen.queryByTestId("agent-cards-none-need-you")).toBeNull();
    expect(document.body).not.toHaveTextContent("Nothing needs you");
    expect(screen.getByTestId("agent-cards-none-need-you-scoped")).toHaveTextContent(`No card needs you. Agents on ${name} are shown as usual: that daemon does not report cards.`);
    expect(screen.queryByTestId("overview-needs-you-count")).toBeNull();
    expect(dashboardMore()).toHaveAccessibleDescription(`No card needs you. Agents on ${name} are not counted: that daemon does not report cards.`);

    view.rerender(<AgentOverview runtime={runtime({ remote: { entries: legacy, catalogEdit: false, cards: false }, status: { "12": "needs_input" } })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("agent-cards-none-need-you-scoped")).toBeNull();
    expect(screen.getByTestId("overview-needs-you-count").textContent).toBe("2");
    expect(dashboardMore()).toHaveAccessibleDescription(`2 agents need you. Agents on ${name} are not counted: that daemon does not report cards.`);
  });

  /// Scenario: With two daemons on the dashboard, a newer one and one from an older release, open each daemon's ⋯ menu, just left of its New agent button. Each is named "More for <daemon>" and holds Card actions, which opens the editor titled "Card actions: <daemon>" with no Daemon picker — editable for the newer daemon, read-only with the explanation for the older one.
  it("desktop/agent-cards/113: each daemon's ⋯ menu opens Card actions bound to that daemon, read-only on an older release", async () => {
    const live = runtime({ remote: { entries: [entry("1", "remote-coder")], catalogEdit: false } });
    render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    const [local, far] = createFixtureFleet("fleet");
    for (const [deckId, name, editable] of [[FIXTURE_DAEMON_ID, deckName(local.connection), true], [FIXTURE_REMOTE_DAEMON_ID, deckName(far.connection), false]] as const) {
      const section = daemonSection(deckId);
      const more = within(section).getByTestId("daemon-more");
      expect(more).toHaveAccessibleName(`More for ${name}`);
      expect(more).toHaveAttribute("title", `More for ${name}`);
      expect(more).toHaveAttribute("aria-haspopup", "menu");
      const newAgent = within(section).queryByTestId("daemon-new-agent");
      if (newAgent) expect(more.closest(".daemon-more")!.nextElementSibling).toBe(newAgent);
      fireEvent.click(more);
      const menu = within(section).getByRole("menu", { name: `More for ${name}` });
      await act(async () => { fireEvent.click(within(menu).getByRole("menuitem", { name: "Card actions" })); });
      const editor = screen.getByRole("dialog", { name: `Card actions: ${name}` });
      expect(within(editor).queryByRole("combobox", { name: "Daemon" })).toBeNull();
      if (editable) {
        expect(within(editor).queryByTestId("agent-card-actions-editor-readonly")).toBeNull();
        expect(live.readCardCatalog).toHaveBeenLastCalledWith(deckId, deckId === FIXTURE_DAEMON_ID ? INSTANCE : REMOTE_INSTANCE);
      } else {
        expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toHaveTextContent(/earlier release, so its card actions cannot be edited from the app/);
      }
      fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));
    }
    expect(live.readCardCatalog).toHaveBeenCalledTimes(1);
  });
});

describe("the Cards view's note when no agent has a card (PR #2491 review)", () => {
  /// Scenario: On the Cards view, the only daemon is from a release without cards and runs agents. The note above the daemons does not promise that every agent gets a card: it says no agent has a card yet and that the daemon's agents are shown as usual because it does not report cards.
  it("desktop/agent-cards/122: with one daemon that does not report cards, the empty Cards view names it rather than promising cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime({ cards: false })} onNavigate={vi.fn()} />);
    const name = deckName(createFixtureFleet("fleet")[0].connection);
    expect(screen.queryByTestId("agent-cards-empty")).toBeNull();
    expect(document.body).not.toHaveTextContent("Every agent the daemon runs gets one");
    expect(screen.getByTestId("agent-cards-empty-scoped")).toHaveTextContent(`No agent has a card yet. Agents on ${name} are shown as usual: that daemon does not report cards.`);
  });

  /// Scenario: On the Cards view, two daemons: one reports cards but has none yet, the other is from a release without cards and runs agents. The note names the daemon without cards instead of saying every agent gets a card; once both daemons report cards and neither has one, the note says no agent has a card yet and that every agent the daemon runs gets one.
  it("desktop/agent-cards/122: in a mixed fleet, the empty Cards view names the daemon without cards, and keeps its promise only when every daemon reports cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    const legacy = [entry("11", "legacy-lead"), entry("12", "legacy-asker")];
    const view = render(<AgentOverview runtime={runtime({ entries: [], remote: { entries: legacy, catalogEdit: false, cards: false } })} onNavigate={vi.fn()} />);
    const name = deckName(createFixtureFleet("fleet")[1].connection);
    expect(screen.queryByTestId("agent-cards-empty")).toBeNull();
    expect(document.body).not.toHaveTextContent("Every agent the daemon runs gets one");
    expect(screen.getByTestId("agent-cards-empty-scoped")).toHaveTextContent(`No agent has a card yet. Agents on ${name} are shown as usual: that daemon does not report cards.`);

    view.rerender(<AgentOverview runtime={runtime({ entries: [], remote: { entries: [], catalogEdit: false } })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("agent-cards-empty-scoped")).toBeNull();
    expect(screen.getByTestId("agent-cards-empty")).toHaveTextContent("No agent has a card yet. Every agent the daemon runs gets one.");
  });
});

describe("what the dashboard says about daemons the cards do not count (PR #2491 review S1)", () => {
  it("names daemons without cards and daemons not answering, in the note and in the ⋯'s description", () => {
    const uncounted = { notReporting: ["old-a", "old-b"], notAnswering: ["far"] };
    expect(uncountedDaemonsText(uncounted, "shown as usual")).toBe("Agents on old-a and old-b are shown as usual: those daemons do not report cards. far is not answering, so its agents are not counted.");
    expect(needsYouCountDescription(1, uncounted)).toBe("1 agent needs you. Agents on old-a and old-b are not counted: those daemons do not report cards. far is not answering, so its agents are not counted.");
    expect(needsYouCountDescription(0, { notReporting: [], notAnswering: ["x", "y", "z"] })).toBe("No card needs you. x, y and z are not answering, so their agents are not counted.");
    expect(needsYouCountDescription(0, NO_UNCOUNTED)).toBeUndefined();
    expect(needsYouCountDescription(3, NO_UNCOUNTED)).toBe("3 agents need you");
  });
});
