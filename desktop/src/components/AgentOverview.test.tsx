import { fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DISPLAY_LIMITS } from "../lib/displayText";
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
import { agentKey, AgentOverview, groupAgents, type OverviewAgent, toOverviewAgent } from "./AgentOverview";

/**
 * Every codepoint the render seam must strip, enumerated rather than sampled —
 * the same fixture shape, and the same reason, as
 * `strip_control_and_bidi_covers_every_bidi_codepoint` in
 * `src/untrusted_text.rs`: a range typo leaves one override live, and one is
 * all a spoof needs. The ANSI escape, NUL, BEL, newline, carriage return, DEL
 * and two C1 controls first; then all twelve bidi formatting and override
 * codepoints, which no "is this a control character" test catches.
 */
const HOSTILE_CODEPOINTS = [
  "\u001b", "\u0000", "\u0007", "\n", "\r", "\u007f", "\u0085", "\u009b",
  "\u202a", "\u202b", "\u202c", "\u202d", "\u202e",
  "\u2066", "\u2067", "\u2068", "\u2069",
  "\u200e", "\u200f", "\u061c",
];

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

/**
 * The agent rows inside `scope`. Each group is a real table, so its `<thead>`
 * contributes a row too — present for a screen reader, and not one of these.
 */
function rows(scope: HTMLElement): HTMLElement[] {
  return within(scope).queryAllByRole("row").filter((row) => row.classList.contains("overview-row"));
}

/** The agent name each row renders, in the order the rows appear. */
function rowNames(scope: HTMLElement): (string | null | undefined)[] {
  return rows(scope).map((row) => row.querySelector(".overview-agent-name strong")?.textContent);
}

/** Every `title` a screen renders — the other half of what a reader sees. */
function titlesOf(container: HTMLElement): string[] {
  return Array.from(container.querySelectorAll("[title]")).map((node) => node.getAttribute("title") ?? "");
}

/** The crowded snapshot with its whole fleet replaced by one hand-built agent. */
function snapshotWithAgent(overrides: Partial<AgentSession>): DeckSnapshot {
  const base = createFixtureSnapshot("crowded");
  const [first] = base.agents;
  return { ...base, agents: [{ ...(first as AgentSession), ...overrides }] };
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
    expect(rows(prd)[0]).toContainElement(coordinator);
  });

  it("marks the start role as coordinator even when it is not the first role", () => {
    renderOverview();

    const dotAi = screen.getByTestId("overview-group-orc-dot-ai");
    expect(rowNames(dotAi)).toEqual(["writer", "reviewer", "orchestrator", "publisher"]);
    expect(rows(dotAi)[2]).toContainElement(within(dotAi).getByText("COORDINATOR"));
  });

  it("buckets mode tabs and untabbed panes into their own groups", () => {
    renderOverview();

    const mode = screen.getByTestId("overview-group-review");
    expect(mode).toHaveAttribute("data-group-kind", "mode");
    expect(rows(mode)).toHaveLength(2);

    const standalone = screen.getByTestId("overview-group-standalone");
    expect(standalone).toHaveAttribute("data-group-kind", "standalone");
    expect(rows(standalone)).toHaveLength(3);
    expect(within(standalone).queryByText("COORDINATOR")).not.toBeInTheDocument();
  });

  it("renders every agent and every group of the crowded scenario", () => {
    const snapshot = createFixtureSnapshot("crowded");
    expect(snapshot.agents).toHaveLength(15);
    renderOverview({ snapshot });

    for (const agent of snapshot.agents) {
      expect(screen.getByTestId(`overview-agent-${agentKey(agent)}`)).toBeVisible();
    }
    expect(rows(document.body)).toHaveLength(15);
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

  it("states the working directory most of a group shares and prints only the rows that differ", () => {
    renderOverview();

    // Every role of this orchestration works in one directory, so the group
    // says it and all six rows stay quiet.
    const prd = screen.getByTestId("overview-group-orc-745");
    expect(prd.querySelector(".overview-group-cwd")).toHaveTextContent("~/code/dot-agent-deck-dispatch-prd-745");
    expect(rows(prd).map((row) => row.querySelector(".overview-cwd")?.textContent)).toEqual(["", "", "", "", "", ""]);

    // The standalone bucket is the case that makes this a DIFFERENCES column
    // rather than a shared-value one: two of its three agents work in the deck
    // checkout and one does not, so the common directory is hoisted and the one
    // row that differs is the only thing printed down the column.
    const standalone = screen.getByTestId("overview-group-standalone");
    expect(standalone.querySelector(".overview-group-cwd")).toHaveTextContent("~/code/dot-agent-deck");
    expect(rows(standalone).map((row) => row.querySelector(".overview-cwd")?.textContent))
      .toEqual(["", "", "~/code/dot-agent-deck/pi-extension"]);
  });

  it("renders working directories home-relative and keeps the full path on hover", () => {
    renderOverview();

    const spike = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "15" })}`);
    const cwd = spike.querySelector(".overview-cwd");
    expect(cwd).toHaveTextContent("~/code/dot-agent-deck/pi-extension");
    // Abbreviated, not hidden: hover still carries the path the daemon reported.
    expect(cwd).toHaveAttribute("title", "/home/dev/code/dot-agent-deck/pi-extension");
    // And no username reaches the screen itself, which is what a screenshot captures.
    expect(document.body.textContent ?? "").not.toContain("/home/dev");
  });

  it("labels the daemon group without printing its socket path, and keeps the raw path as the key", () => {
    const { container } = renderOverview();

    const label = screen.getByTestId("daemon-group").querySelector(".daemon-identity code");
    expect(label).toHaveTextContent("dot-agent-deck.sock");
    expect(label).toHaveAttribute("title", FIXTURE_DAEMON_ID);
    // The identity key is still the raw socket path — the label is display only.
    expect(screen.getByTestId("daemon-group")).toHaveAttribute("data-daemon-id", FIXTURE_DAEMON_ID);
    expect(container.textContent ?? "").not.toContain("/tmp/");
  });

  it("strips every control and bidi character out of what it renders and out of its titles", () => {
    const hostile = HOSTILE_CODEPOINTS.join("");
    const { container } = renderOverview({
      snapshot: snapshotWithAgent({
        displayName: `pwn${hostile}ed`,
        cli: `claude${hostile}_code`,
        cwd: `/home/dev/code${hostile}/deck`,
        activeTool: `ed${hostile}it`,
        activeToolDetail: `src/${hostile}main.rs`,
        tab: { kind: "orchestration", orchestrationId: "orc-x", name: `orc${hostile}`, displayTitle: `Orc${hostile}A`, roleName: `cod${hostile}er`, roleIndex: 0, isStartRole: true },
      }),
    });

    // Joined with a character that is not itself under test, so the assertion
    // cannot be satisfied or defeated by the separator.
    const rendered = [container.textContent ?? "", ...titlesOf(container)].join(" ~ ");
    for (const codepoint of HOSTILE_CODEPOINTS) {
      const name = `U+${codepoint.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0")}`;
      expect(rendered.includes(codepoint), `${name} survived the render seam`).toBe(false);
    }
    // Stripped, not blanked: every legitimate character is still there.
    expect(container.querySelector(".overview-agent-name strong")).toHaveTextContent("pwned");
    expect(within(screen.getByTestId("overview-group-orc-x")).getByRole("heading")).toHaveTextContent("OrcA");
  });

  it("keeps zero-width joiners, which cannot reorder anything", () => {
    // The recorded decision behind `lib/displayText`: the seam mirrors the Rust
    // policy exactly rather than widening it, because ZWJ and ZWNJ are
    // load-bearing in emoji sequences and in several scripts, and neither can
    // produce the bidi spoof the filter exists to stop.
    const joined = "team \u{1f468}\u200d\u{1f4bb}";
    const { container } = renderOverview({ snapshot: snapshotWithAgent({ displayName: joined }) });

    expect(container.querySelector(".overview-agent-name strong")?.textContent).toBe(joined);
  });

  it("bounds every string it renders, however long the daemon's is", () => {
    const long = "x".repeat(600);
    const { container } = renderOverview({
      snapshot: snapshotWithAgent({
        displayName: long,
        cli: long,
        cwd: `/home/dev/${long}`,
        activeTool: long,
        // For a shell tool this is the first command line the agent ran, so it
        // is the value most worth keeping short and out of a screenshot.
        activeToolDetail: `cargo run -- --token ${long}`,
        tab: { kind: "dashboard" },
      }),
    });

    // One over each budget: the clamp appends an elision marker, so a truncated
    // value never passes itself off as complete.
    const lengthOf = (selector: string) => Array.from(container.querySelector(selector)?.textContent ?? "").length;
    expect(lengthOf(".overview-agent-name strong")).toBe(DISPLAY_LIMITS.name + 1);
    expect(lengthOf(".overview-cli")).toBe(DISPLAY_LIMITS.name + 1);
    expect(lengthOf(".overview-cwd")).toBe(DISPLAY_LIMITS.path + 1);
    expect(lengthOf(".overview-tool strong")).toBe(DISPLAY_LIMITS.toolName + 1);
    expect(lengthOf(".overview-tool em")).toBe(DISPLAY_LIMITS.toolDetail + 1);
    for (const title of titlesOf(container)) {
      expect(Array.from(title).length).toBeLessThanOrEqual(DISPLAY_LIMITS.title + 1);
    }
  });

  it("exposes real column headers so a screen reader can name the column a cell is in", () => {
    renderOverview();

    const prd = screen.getByTestId("overview-group-orc-745");
    const table = within(prd).getByRole("table");
    expect(within(table).getAllByRole("columnheader").map((header) => header.textContent))
      .toEqual(["Status", "Agent", "State", "CLI", "Active tool", "Tools", "Working directory"]);
    for (const row of rows(prd)) expect(within(row).getAllByRole("cell")).toHaveLength(7);
    // The visible legend is decoration for the shared grid and stays out of the
    // accessibility tree, so the columns are not announced twice.
    expect(document.querySelector(".overview-legend")).toHaveAttribute("aria-hidden", "true");
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

  it("says the control channel is still opening rather than listing a fleet it has not read", () => {
    const snapshot = createFixtureSnapshot("crowded");
    snapshot.connection = { status: "loading", socketPath: FIXTURE_DAEMON_ID, message: "Connecting to the daemon" };
    renderOverview({ snapshot });

    expect(screen.getByTestId("overview-loading")).toBeVisible();
    expect(screen.getByRole("heading", { name: "Establishing control channel" })).toBeVisible();
    expect(rows(document.body)).toHaveLength(0);
    expect(screen.queryByTestId("overview-first-run")).not.toBeInTheDocument();
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
    expect(rows(document.body)).toHaveLength(0);
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
  const crowded = () => createFixtureSnapshot("crowded").agents.map(toOverviewAgent);

  it("puts the standalone bucket last however the agents arrive", () => {
    const groups = groupAgents([...crowded()].reverse());

    expect(groups.map((group) => group.kind)).toEqual(["orchestration", "orchestration", "mode", "standalone"]);
    expect(groups.at(-1)?.id).toBe("standalone");
  });

  it("omits the standalone bucket when nothing is untabbed", () => {
    const agents = crowded().filter((agent) => agent.tab.kind !== "dashboard");

    expect(groupAgents(agents).some((group) => group.kind === "standalone")).toBe(false);
  });

  it("keeps two orchestrations apart when neither reports an id", () => {
    // `orchestrationId` is optional on the wire. Keying on the name instead
    // would fold two unrelated orchestrations into one card carrying two 01s.
    const [seed] = crowded();
    const nameless = (id: string): OverviewAgent => ({
      ...seed,
      id,
      tab: { kind: "orchestration", name: "dot-ai", displayTitle: "dot-ai", roleName: "writer", roleIndex: 0, isStartRole: true },
    });
    const groups = groupAgents([nameless("1"), nameless("2")]);

    expect(groups).toHaveLength(2);
    expect(groups.map((group) => group.agents.length)).toEqual([1, 1]);
  });

  it("hoists a working directory only when at least two members share it", () => {
    const [seed] = crowded();
    const at = (id: string, cwd: string): OverviewAgent => ({ ...seed, id, cwd, tab: { kind: "dashboard" } });

    // All different: every row is a difference, so there is nothing to hoist.
    expect(groupAgents([at("1", "/a"), at("2", "/b"), at("3", "/c")])[0]?.commonCwd).toBeUndefined();
    // A group of one: its row already prints the value, and stating it in the
    // header as well would be the same path twice on one card.
    expect(groupAgents([at("1", "/a")])[0]?.commonCwd).toBeUndefined();
    // Two of three: the majority is hoisted and the odd one out stands proud.
    expect(groupAgents([at("1", "/a"), at("2", "/b"), at("3", "/a")])[0]?.commonCwd).toBe("/a");
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
