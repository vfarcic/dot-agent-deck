import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS, mapDesktopSnapshot, type DesktopAgentDto, type DesktopSettingsDto, type PullRequestInfoDto } from "./lib/bridge";
import { PrBrowserHostContext, type PrBrowserHost } from "./lib/prBrowser";
import type { AgentCardCheckedRef, AgentCardEntry, AgentCardsSnapshot, DeckRuntimeState } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/*
 * Issue #2418 review of aa9d818e — `desktop/agent-cards/026`. The same pull
 * request or issue can be reported by two cards, or be both a card's checked
 * reference and an agent's own pull request. The in-app browser keeps one
 * page per address, so a second accepted open of the address already on
 * screen does not reload it; what it changes is WHO the browser is for: its
 * name, its deck and agent, and whether a card owns it, so the browser
 * closes with what asked for it last.
 *
 * Every open below goes through the real shell's `OpenCardReference` and
 * `OpenPullRequest` providers, from the dashboard's own controls; only the
 * native page host and the daemons' pushes are synthetic.
 */

const DECK_A = "deck-00000000000000a1";
const DECK_B = "deck-00000000000000b2";
const ISSUE_URL = "https://github.com/o/r/issues/3";
const PR_URL = "https://github.com/vfarcic/dot-agent-deck/pull/12345";
const MODE_KEY = "dot-agent-deck.overview.mode";

function fakeHost() {
  return {
    available: true,
    open: vi.fn(async () => undefined),
    setBounds: vi.fn(async () => undefined),
    setVisible: vi.fn(async () => undefined),
    back: vi.fn(async () => undefined),
    scroll: vi.fn(async () => undefined),
    openExternal: vi.fn(async () => undefined),
    close: vi.fn(async () => undefined),
    signOut: vi.fn(async () => undefined),
    onClosed: vi.fn(() => () => undefined),
  } satisfies PrBrowserHost;
}

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => { document = structuredClone(next); return structuredClone(document); }),
  };
}

function agent(id: string, pullRequest?: PullRequestInfoDto): DesktopAgentDto {
  return { id, displayName: `Coder ${id}`, cwd: "/tmp/project", rows: 32, cols: 120, agentType: "claude_code", status: "working", toolCount: 3, tab: { kind: "dashboard" }, ...(pullRequest ? { pullRequest } : {}) };
}

const ISSUE: AgentCardCheckedRef = { field: "issue", kind: "issue", claim: "o/r#3", repo: "o/r", number: 3, url: ISSUE_URL, result: { status: "checked", state: "open", checked_at: "2026-11-20T10:00:00Z", stale: false } };
const PULL: AgentCardCheckedRef = { field: "pull-request", kind: "pull_request", claim: "#12345", repo: "vfarcic/dot-agent-deck", number: 12345, url: PR_URL, result: { status: "checked", state: "open", checked_at: "2026-11-20T10:00:00Z", stale: false } };

/** A root card for `agentId` on `deckId`, carrying `refs` as the deck checked them. */
function card(deckId: string, agentId: string, name: string, refs: AgentCardCheckedRef[]): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`, instance_id: `instance-${deckId}`, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "working", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [], primary_action_id: "", facts: { source: "deck", pull_request: null },
    checked_refs: refs, revision: 1, updated_at: "2026-11-20T10:00:00Z",
  };
}

function cards(deckId: string, entries: AgentCardEntry[]): AgentCardsSnapshot {
  return { deckId, instanceId: `instance-${deckId}`, revision: 1, entries, catalogErrors: [], tree: true };
}

function deck(deckId: string, agents: DesktopAgentDto[]) {
  return mapDesktopSnapshot({
    connection: { status: "connected", deckId, socketPath: `/tmp/${deckId}.sock`, deckKind: "local", clientProtocolVersion: 8, serverProtocolVersion: 8, clientBuildVersion: "0.1.0", daemonBuildVersion: "0.1.0" },
    agents,
    protocolVersion: 8,
    source: "daemon",
  });
}

type Scene = { decks: { deckId: string; agents: DesktopAgentDto[] }[]; agentCards: Record<string, AgentCardsSnapshot> };

function runtime(scene: Scene): DeckRuntimeState {
  const fleet = scene.decks.map((entry) => deck(entry.deckId, entry.agents));
  const settings = settingsStore();
  return {
    mode: "live",
    snapshot: fleet[0],
    fleet,
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    agentCards: scene.agentCards,
  } as unknown as DeckRuntimeState;
}

/** The real shell on the dashboard, and a way to push a later state of the same decks. */
function renderShell(host: PrBrowserHost, scene: Scene) {
  const view = render(
    <PrBrowserHostContext.Provider value={host}>
      <DeckShell runtime={runtime(scene)} initialView={{ kind: "overview" }} />
    </PrBrowserHostContext.Provider>,
  );
  return {
    push: async (next: Scene) => {
      view.rerender(
        <PrBrowserHostContext.Provider value={host}>
          <DeckShell runtime={runtime(next)} initialView={{ kind: "overview" }} />
        </PrBrowserHostContext.Provider>,
      );
      await act(async () => { await Promise.resolve(); });
    },
  };
}

function cardOf(agentId: string): HTMLElement {
  const found = document.querySelector<HTMLElement>(`article[data-card-key="pane-${agentId}/${agentId}"]`);
  if (!found) throw new Error(`no card for ${agentId}`);
  return found;
}

/** Click a card's badge. The browser over the dashboard makes it inert to a person; `fireEvent` reaches it anyway, standing in for a voice open that settles after another (the review's case). */
async function clickCardBadge(agentId: string, testId: "agent-card-issue" | "agent-card-pr") {
  await act(async () => { fireEvent.click(within(cardOf(agentId)).getAllByTestId(testId)[0]); });
}

async function chooseMode(mode: "table" | "cards") {
  await act(async () => { fireEvent.click(screen.getByTestId(`overview-mode-${mode}`)); });
}

const browserName = () => screen.getByTestId("pr-browser").getAttribute("aria-label");

describe("desktop/agent-cards/026 — a reference opened again follows what asked for it last (issue #2418)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    window.localStorage.setItem(MODE_KEY, "cards");
  });
  afterEach(() => cleanup());

  /// Scenario: bench's and issue-queue's cards both report issue o/r#3. Open it from bench's card, then from issue-queue's: the page is not reloaded and the browser is named for issue-queue. A list without bench's card leaves it open; a list without issue-queue's card closes it.
  it("desktop/agent-cards/026: a second card opening the same reference takes the browser over", async () => {
    const host = fakeHost();
    const agents = [agent("1"), agent("2")];
    const both = { decks: [{ deckId: DECK_A, agents }], agentCards: { [DECK_A]: cards(DECK_A, [card(DECK_A, "1", "bench", [ISSUE]), card(DECK_A, "2", "issue-queue", [ISSUE])]) } };
    const shell = renderShell(host, both);

    await clickCardBadge("1", "agent-card-issue");
    expect(browserName()).toBe("Issue #3 for bench");
    await clickCardBadge("2", "agent-card-issue");
    expect(browserName()).toBe("Issue #3 for issue-queue");
    expect(host.open, "the page already on that issue is not reloaded").toHaveBeenCalledTimes(1);

    await shell.push({ ...both, agentCards: { [DECK_A]: cards(DECK_A, [card(DECK_A, "2", "issue-queue", [ISSUE])]) } });
    expect(screen.getByTestId("pr-browser"), "bench's card going does not close issue-queue's browser").toBeInTheDocument();
    expect(host.close).not.toHaveBeenCalled();

    await shell.push({ ...both, agentCards: { [DECK_A]: cards(DECK_A, []) } });
    expect(host.close).toHaveBeenCalled();
    expect(screen.queryByTestId("pr-browser")).toBeNull();
  });

  /// Scenario: a card on the first deck and a card on a second deck both report issue o/r#3. Open it from the first deck's card, then from the second's: the browser is named for the second deck's card, the first deck dropping its card leaves it open, and the second deck dropping its card closes it.
  it("desktop/agent-cards/026: a card on another deck opening the same reference takes the browser over", async () => {
    const host = fakeHost();
    const decks = [{ deckId: DECK_A, agents: [agent("1")] }, { deckId: DECK_B, agents: [agent("1")] }];
    const both = { decks, agentCards: { [DECK_A]: cards(DECK_A, [card(DECK_A, "1", "bench", [ISSUE])]), [DECK_B]: cards(DECK_B, [card(DECK_B, "1", "far-bench", [ISSUE])]) } };
    const shell = renderShell(host, both);

    const deckCard = (deckId: string) => {
      const found = document.querySelector<HTMLElement>(`[data-testid="agent-cards-deck-${deckId}"] article[data-card-key="pane-1/1"]`)
        ?? [...document.querySelectorAll<HTMLElement>(`article[data-card-key="pane-1/1"]`)].find((article) => within(article).queryByText(deckId === DECK_A ? "bench" : "far-bench"));
      if (!found) throw new Error(`no card on ${deckId}`);
      return found;
    };
    await act(async () => { fireEvent.click(within(deckCard(DECK_A)).getByTestId("agent-card-issue")); });
    expect(browserName()).toBe("Issue #3 for bench");
    await act(async () => { fireEvent.click(within(deckCard(DECK_B)).getByTestId("agent-card-issue")); });
    expect(browserName()).toBe("Issue #3 for far-bench");
    expect(host.open).toHaveBeenCalledTimes(1);

    await shell.push({ decks, agentCards: { ...both.agentCards, [DECK_A]: cards(DECK_A, []) } });
    expect(screen.getByTestId("pr-browser"), "the first deck dropping its card does not close the second deck's browser").toBeInTheDocument();

    await shell.push({ decks, agentCards: { [DECK_A]: cards(DECK_A, []), [DECK_B]: cards(DECK_B, []) } });
    expect(screen.queryByTestId("pr-browser")).toBeNull();
  });

  /// Scenario: agent 1 has pull request #12345 and its card reports the same one. Open it from the card, switch the dashboard to the table, and open it from the agent's pull request badge: the browser is named for the agent, stays open when the card goes, and closes when the agent goes.
  it("desktop/agent-cards/026: the pull request badge opening a card's reference gives it the agent's lifetime", async () => {
    const host = fakeHost();
    const withPr = agent("1", { number: 12345, url: PR_URL, state: "open", review: "review_required" });
    const scene = { decks: [{ deckId: DECK_A, agents: [withPr] }], agentCards: { [DECK_A]: cards(DECK_A, [card(DECK_A, "1", "bench", [PULL])]) } };
    const shell = renderShell(host, scene);

    await clickCardBadge("1", "agent-card-pr");
    expect(browserName()).toBe("Pull request #12345 for bench");
    await chooseMode("table");
    await act(async () => { fireEvent.click(screen.getByTestId("overview-pr-badge")); });
    expect(browserName()).toBe("Pull request #12345 for Coder 1");
    expect(host.open).toHaveBeenCalledTimes(1);

    await shell.push({ ...scene, agentCards: { [DECK_A]: cards(DECK_A, []) } });
    expect(screen.getByTestId("pr-browser"), "the card going does not close a browser the agent's badge opened").toBeInTheDocument();

    await shell.push({ decks: [{ deckId: DECK_A, agents: [] }], agentCards: { [DECK_A]: cards(DECK_A, []) } });
    expect(screen.queryByTestId("pr-browser")).toBeNull();
  });

  /// Scenario: agent 1 has pull request #12345 and its card reports the same one. Open it from the agent's badge in the table, switch to cards, and open it from the card: the browser is named for the card, stays open when the agent stops but its card stays, and closes when the card goes.
  it("desktop/agent-cards/026: a card opening the badge's pull request gives it the card's lifetime", async () => {
    window.localStorage.setItem(MODE_KEY, "table");
    const host = fakeHost();
    const withPr = agent("1", { number: 12345, url: PR_URL, state: "open", review: "review_required" });
    const scene = { decks: [{ deckId: DECK_A, agents: [withPr] }], agentCards: { [DECK_A]: cards(DECK_A, [card(DECK_A, "1", "bench", [PULL])]) } };
    const shell = renderShell(host, scene);

    await act(async () => { fireEvent.click(screen.getByTestId("overview-pr-badge")); });
    expect(browserName()).toBe("Pull request #12345 for Coder 1");
    await chooseMode("cards");
    await clickCardBadge("1", "agent-card-pr");
    expect(browserName()).toBe("Pull request #12345 for bench");
    expect(host.open).toHaveBeenCalledTimes(1);

    await shell.push({ ...scene, decks: [{ deckId: DECK_A, agents: [] }] });
    expect(screen.getByTestId("pr-browser"), "the agent stopping does not close a browser its card now owns").toBeInTheDocument();

    await shell.push({ decks: [{ deckId: DECK_A, agents: [] }], agentCards: { [DECK_A]: cards(DECK_A, []) } });
    expect(screen.queryByTestId("pr-browser")).toBeNull();
  });
});
