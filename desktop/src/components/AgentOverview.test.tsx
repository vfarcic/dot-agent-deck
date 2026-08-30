import { fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import type { AgentSession, DeckRuntimeState, DeckSnapshot } from "../types";

/**
 * The overview's central claim is that it mounts no terminal. Spying on the
 * mock rather than only querying the DOM makes that a POSITIVE assertion: a
 * `TerminalViewport` that mounted and immediately unmounted, or one rendered
 * off-screen, still trips this.
 */
const terminalMounted = vi.fn();
vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => {
    terminalMounted(agentId);
    return <pre data-testid={`terminal-${agentId}`}>terminal</pre>;
  },
}));

import { DeckShell } from "../App";
import { agentKey, AgentOverview, groupAgents, toOverviewAgent } from "./AgentOverview";

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "fixture",
    snapshot: createFixtureSnapshot("crowded"),
    terminalData: {},
    runAction: vi.fn(async () => ({ ok: true }) as import("../types").DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    ...overrides,
  };
}

function renderOverview(overrides: Partial<DeckRuntimeState> = {}) {
  return render(<AgentOverview runtime={runtime(overrides)} onNavigate={vi.fn()} />);
}

/** The agent name each row renders, in the order the rows appear. */
function rowNames(scope: HTMLElement): (string | null | undefined)[] {
  return within(scope).getAllByRole("listitem").map((row) => row.querySelector(".overview-agent-name strong")?.textContent);
}

describe("AgentOverview", () => {
  beforeEach(() => {
    terminalMounted.mockClear();
    window.localStorage.clear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })));
  });

  afterEach(() => vi.unstubAllGlobals());

  it("renders an orchestration as one unit, in role order, with the coordinator identifiable", () => {
    renderOverview();

    const prd = screen.getByTestId("overview-group-orc-745");
    expect(prd).toHaveAttribute("data-group-kind", "orchestration");
    expect(within(prd).getByRole("heading", { name: "PRD #745 · agent overview" })).toBeVisible();
    // The fixture declares these six out of role order on purpose.
    expect(rowNames(prd)).toEqual(["orchestrator", "coder", "tester", "reviewer", "docs", "release"]);
    const coordinator = within(prd).getByText("COORDINATOR");
    expect(within(prd).getAllByText("COORDINATOR")).toHaveLength(1);
    expect(within(prd).getAllByRole("listitem")[0]).toContainElement(coordinator);
  });

  it("marks the start role as coordinator even when it is not the first role", () => {
    renderOverview();

    const dotAi = screen.getByTestId("overview-group-orc-dot-ai");
    expect(rowNames(dotAi)).toEqual(["writer", "reviewer", "orchestrator", "publisher"]);
    expect(within(dotAi).getAllByRole("listitem")[2]).toContainElement(within(dotAi).getByText("COORDINATOR"));
  });

  it("buckets mode tabs and untabbed panes into their own groups", () => {
    renderOverview();

    const mode = screen.getByTestId("overview-group-review");
    expect(mode).toHaveAttribute("data-group-kind", "mode");
    expect(within(mode).getAllByRole("listitem")).toHaveLength(2);

    const standalone = screen.getByTestId("overview-group-standalone");
    expect(standalone).toHaveAttribute("data-group-kind", "standalone");
    expect(within(standalone).getAllByRole("listitem")).toHaveLength(3);
    expect(within(standalone).queryByText("COORDINATOR")).not.toBeInTheDocument();
  });

  it("renders every agent and every group of the crowded scenario", () => {
    const snapshot = createFixtureSnapshot("crowded");
    expect(snapshot.agents).toHaveLength(15);
    renderOverview({ snapshot });

    for (const agent of snapshot.agents) {
      expect(screen.getByTestId(`overview-agent-${agentKey(agent)}`)).toBeVisible();
    }
    expect(screen.getAllByRole("listitem")).toHaveLength(15);
    expect(screen.getAllByRole("article")).toHaveLength(4);
    expect(screen.getByTestId("overview-count-agents")).toHaveTextContent("15");
    expect(screen.getByTestId("daemon-group")).toHaveAttribute("data-daemon-id", FIXTURE_DAEMON_ID);
  });

  it("carries the honest columns the daemon actually reports", () => {
    renderOverview();

    const coder = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "2" })}`);
    expect(coder).toHaveTextContent("coder");
    expect(coder).toHaveTextContent("running");
    expect(coder).toHaveTextContent("claude_code");
    expect(coder).toHaveTextContent("edit");
    expect(coder).toHaveTextContent("desktop/src/components/AgentOverview.tsx");
    expect(coder).toHaveTextContent("132");

    // An agent with no active tool says so rather than borrowing a placeholder.
    const docs = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "5" })}`);
    expect(docs).toHaveTextContent("no active tool");
  });

  it("states a group's shared working directory once and prints only the rows that differ", () => {
    renderOverview();

    // Every role of this orchestration works in one directory, so the group
    // says it and its rows stay quiet.
    const prd = screen.getByTestId("overview-group-orc-745");
    expect(prd.querySelector(".overview-group-cwd")).toHaveTextContent("/home/vfarcic/code/dot-agent-deck-dispatch-prd-745");
    for (const row of within(prd).getAllByRole("listitem")) {
      expect(row.querySelector(".overview-cwd")).toHaveTextContent("");
    }

    // The standalone bucket does not share one, so every row prints its own and
    // the odd directory out is the thing that stands proud.
    const standalone = screen.getByTestId("overview-group-standalone");
    expect(standalone.querySelector(".overview-group-cwd")).toBeNull();
    expect(within(standalone).getByText("/home/vfarcic/code/dot-agent-deck/pi-extension")).toBeVisible();
  });

  it("shows nothing the daemon cannot report", () => {
    const snapshot = createFixtureSnapshot("crowded");
    // Crowded agents carry live mode's own placeholders in every field the
    // daemon does not have, so a leak onto the screen shows up as this string.
    expect(snapshot.agents[0]?.model).toBe("Unavailable");
    expect(snapshot.agents[0]?.worktree).toBe("Unavailable");

    const { container } = renderOverview({ snapshot });
    const text = container.textContent ?? "";
    expect(text).not.toContain("Unavailable");
    expect(text).not.toMatch(/\$\d/);
    expect(text).not.toContain("TOKENS");
    expect(text).not.toContain("CONTEXT");
    expect(text).not.toContain("ATT ");
  });

  it("keys agents by (daemonId, agentId) so two daemons minting id 1 do not collide", () => {
    expect(agentKey({ daemonId: "/tmp/a.sock", id: "1" })).not.toBe(agentKey({ daemonId: "/tmp/b.sock", id: "1" }));

    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    const twoDaemons: DeckSnapshot = {
      ...base,
      agents: [
        { ...(first as AgentSession), id: "1", daemonId: "/tmp/a.sock", displayName: "alpha", tab: { kind: "dashboard" } },
        { ...(first as AgentSession), id: "1", daemonId: "/tmp/b.sock", displayName: "beta", tab: { kind: "dashboard" } },
      ],
    };
    renderOverview({ snapshot: twoDaemons });

    expect(screen.getByTestId(`overview-agent-${agentKey({ daemonId: "/tmp/a.sock", id: "1" })}`)).toHaveTextContent("alpha");
    expect(screen.getByTestId(`overview-agent-${agentKey({ daemonId: "/tmp/b.sock", id: "1" })}`)).toHaveTextContent("beta");
  });

  it("mounts no terminal for any agent", () => {
    const { container } = renderOverview();

    expect(terminalMounted).not.toHaveBeenCalled();
    expect(screen.queryAllByTestId(/^terminal-/)).toHaveLength(0);
    expect(container.querySelectorAll(".terminal-viewport, .agent-panel, .agent-tile, canvas")).toHaveLength(0);
  });

  it("treats a connected daemon with zero agents as the first run, not as a failure", () => {
    const snapshot = createFixtureSnapshot("empty");
    // The fixture used to send `empty` down the disconnected branch, which made
    // the genuine first-run screen impossible to look at (PRD #745 M2).
    expect(snapshot.connection.status).toBe("connected");
    expect(snapshot.agents).toHaveLength(0);

    renderOverview({ snapshot });

    expect(screen.getByTestId("overview-first-run")).toBeVisible();
    expect(screen.getByRole("heading", { name: "No agents are running yet" })).toBeVisible();
    expect(screen.queryByTestId("overview-disconnected")).not.toBeInTheDocument();
    expect(screen.queryByTestId("overview-incompatible")).not.toBeInTheDocument();
  });

  it("says what happened when the daemon is unreachable", () => {
    renderOverview({ snapshot: createFixtureSnapshot("disconnected") });

    const note = screen.getByTestId("overview-disconnected");
    expect(note).toBeVisible();
    expect(within(note).getByRole("heading", { name: "Daemon disconnected" })).toBeVisible();
    expect(within(note).getByText(/No dot-agent-deck daemon is listening/)).toBeVisible();
    expect(screen.queryByTestId("overview-first-run")).not.toBeInTheDocument();
  });

  it("refuses to imply a fleet it cannot read from an incompatible daemon", () => {
    const snapshot = createFixtureSnapshot("error");
    snapshot.connection = { ...snapshot.connection, daemonDetected: true, runningAgentCount: 3 };
    renderOverview({ snapshot });

    expect(screen.getByTestId("overview-incompatible")).toBeVisible();
    expect(screen.getByRole("heading", { name: "Incompatible daemon" })).toBeVisible();
    expect(screen.getByText(/reports 3 running agents/)).toBeVisible();
    expect(screen.queryAllByRole("listitem")).toHaveLength(0);
    expect(screen.queryByTestId("overview-first-run")).not.toBeInTheDocument();
  });

  it("reconnects on demand", () => {
    const deck = runtime();
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} />);

    fireEvent.click(screen.getByTestId("overview-refresh"));
    expect(deck.reconnect).toHaveBeenCalled();
  });
});

describe("groupAgents", () => {
  it("puts the standalone bucket last however the agents arrive", () => {
    const agents = createFixtureSnapshot("crowded").agents.map(toOverviewAgent);
    const groups = groupAgents([...agents].reverse());

    expect(groups.map((group) => group.kind)).toEqual(["orchestration", "orchestration", "mode", "standalone"]);
    expect(groups.at(-1)?.id).toBe("standalone");
  });

  it("omits the standalone bucket when nothing is untabbed", () => {
    const agents = createFixtureSnapshot("crowded").agents
      .map(toOverviewAgent)
      .filter((agent) => agent.tab.kind !== "dashboard");

    expect(groupAgents(agents).some((group) => group.kind === "standalone")).toBe(false);
  });
});

describe("DeckShell", () => {
  beforeEach(() => {
    terminalMounted.mockClear();
    window.localStorage.clear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })));
  });

  afterEach(() => vi.unstubAllGlobals());

  it("opens on the deck and reaches the overview from the rail without mounting a terminal", () => {
    render(<DeckShell runtime={runtime({ snapshot: createFixtureSnapshot("connected") })} />);

    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expect(screen.queryByTestId("daemon-group")).not.toBeInTheDocument();

    terminalMounted.mockClear();
    fireEvent.click(screen.getByTestId("open-overview"));

    expect(screen.getByTestId("daemon-group")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
    expect(terminalMounted).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("open-deck"));
    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expect(screen.queryByTestId("daemon-group")).not.toBeInTheDocument();
  });
});
