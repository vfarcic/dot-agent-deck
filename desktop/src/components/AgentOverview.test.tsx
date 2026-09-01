import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DISPLAY_LIMITS } from "../lib/displayText";
import { UNREPORTED } from "../types";
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
import { agentDomKey, agentKey, AgentOverview, anonymousOrchestrationKey, groupAgents, groupKey, hoistedCwdOf, type OverviewAgent, type OverviewGroupKind, toOverviewAgent } from "./AgentOverview";

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
    setShownTerminals: vi.fn(async () => undefined),
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

/**
 * The identity-bearing attributes a rendered screen carries. These are the half
 * of the render seam the text and `title` sweeps never reach, which is how raw,
 * unbounded daemon strings kept arriving here after both of those were closed.
 */
const IDENTITY_ATTRIBUTES = ["id", "data-testid", "data-group-id", "data-daemon-id", "aria-labelledby"];

function identityAttributes(container: HTMLElement): { name: string; value: string }[] {
  return Array.from(container.querySelectorAll("*")).flatMap((node) =>
    IDENTITY_ATTRIBUTES.flatMap((name) => {
      const value = node.getAttribute(name);
      return value === null ? [] : [{ name, value }];
    }),
  );
}

/** One group's card, addressed exactly the way the component keys it. */
function groupCard(kind: OverviewGroupKind, id: string): HTMLElement {
  return screen.getByTestId(`overview-group-${groupKey(kind, id)}`);
}

/** Every fleet instrument in the header, in the order the top bar shows them. */
const COUNTERS = ["agents", "running", "waiting", "failed", "groups"] as const;

function counterText(): string[] {
  return COUNTERS.map((name) => screen.getByTestId(`overview-count-${name}`).querySelector("strong")?.textContent ?? "");
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

    const prd = groupCard("orchestration", "orc-745");
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

    const dotAi = groupCard("orchestration", "orc-dot-ai");
    expect(rowNames(dotAi)).toEqual(["writer", "reviewer", "orchestrator", "publisher"]);
    expect(rows(dotAi)[2]).toContainElement(within(dotAi).getByText("COORDINATOR"));
  });

  it("buckets mode tabs and untabbed panes into their own groups", () => {
    renderOverview();

    const mode = groupCard("mode", "review");
    expect(mode).toHaveAttribute("data-group-kind", "mode");
    expect(rows(mode)).toHaveLength(2);

    const standalone = groupCard("standalone", "standalone");
    expect(standalone).toHaveAttribute("data-group-kind", "standalone");
    expect(rows(standalone)).toHaveLength(3);
    expect(within(standalone).queryByText("COORDINATOR")).not.toBeInTheDocument();
  });

  /**
   * Scenario: render the crowded overview and read the group cards in document
   * order. The standalone card must lead, ahead of both orchestration cards and
   * the mode card — the pure-function order proves what `groupAgents` returns,
   * this proves the screen a user actually sees matches it.
   */
  it("presents the standalone card first on the screen, not only first in the grouping", () => {
    renderOverview();

    const cards = screen.getAllByRole("article");
    expect(cards.map((card) => [card.getAttribute("data-group-kind"), card.getAttribute("data-group-id")])).toEqual([
      ["standalone", "standalone"],
      ["orchestration", "orc-745"],
      ["orchestration", "orc-dot-ai"],
      ["mode", "review"],
    ]);

    // Document position, not query order: the leading card genuinely precedes
    // every other card in the rendered tree.
    const [leading, ...rest] = cards;
    for (const card of rest) {
      expect(leading.compareDocumentPosition(card) & Node.DOCUMENT_POSITION_FOLLOWING).toBeGreaterThan(0);
    }
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
    expect(counterText()).toEqual(["15", "6", "8", "1", "4"]);
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
    const prd = groupCard("orchestration", "orc-745");
    expect(prd.querySelector(".overview-group-cwd")).toHaveTextContent("~/code/dot-agent-deck-dispatch-prd-745");
    expect(rows(prd).map((row) => row.querySelector(".overview-cwd")?.textContent)).toEqual(["", "", "", "", "", ""]);

    // The standalone bucket is the case that makes this a DIFFERENCES column
    // rather than a shared-value one: two of its three agents work in the deck
    // checkout and one does not, so the common directory is hoisted and the one
    // row that differs is the only thing printed down the column.
    const standalone = groupCard("standalone", "standalone");
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
    expect(within(groupCard("orchestration", "orc-x")).getByRole("heading")).toHaveTextContent("OrcA");
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

  /**
   * Scenario: render a fleet whose daemon reports a 50 000-character socket
   * path, agent ids and mode name, then read every identity attribute the
   * screen produced. Each must be bounded, and the two rows must stay two rows.
   */
  it("bounds every identity that reaches a DOM attribute, however long the daemon's is", () => {
    // `DesktopAgentDto.id` has no frontend validation and no clamp — it is
    // bounded only by the 16 MiB protocol frame, and `encodeURIComponent`
    // expands it by up to three — so an unbounded copy in a `data-*`, a DOM id
    // or a React key lets a malformed daemon freeze the webview on every
    // snapshot. The text and `title` sweeps do not reach any of these.
    const long = "y".repeat(50_000);
    const socketPath = `/tmp/${long}.sock`;
    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    const alpha: AgentSession = { ...(first as AgentSession), id: `${long}-1`, daemonId: socketPath, displayName: "alpha", tab: { kind: "mode", name: `mode-${long}` } };
    const beta: AgentSession = { ...alpha, id: `${long}-2`, displayName: "beta" };
    const { container } = renderOverview({ snapshot: { ...base, connection: { ...base.connection, socketPath }, agents: [alpha, beta] } });

    const attributes = identityAttributes(container);
    // The screen really did render these: an empty sweep would pass while
    // proving nothing.
    expect(attributes.length).toBeGreaterThan(5);
    // The budget, plus the longest fixed prefix the component prepends
    // (`overview-group-title-`) and the digest suffix an over-budget value
    // keeps. Against 50 000 the exact slack does not matter; being in the
    // hundreds rather than the tens of thousands is the whole assertion.
    const ceiling = DISPLAY_LIMITS.domIdentity + 64;
    for (const { name, value } of attributes) {
      expect(Array.from(value).length, `${name} reached the DOM unbounded`).toBeLessThanOrEqual(ceiling);
    }

    // Bounded, and still two agents: the clamp must not fold two rows whose ids
    // share a 50 000-character prefix into one.
    expect(agentDomKey(alpha)).not.toBe(agentDomKey(beta));
    expect(rows(container)).toHaveLength(2);
    expect(new Set(rows(container).map((row) => row.getAttribute("data-testid"))).size).toBe(2);
    expect(rowNames(container)).toEqual(["alpha", "beta"]);
  });

  it("keeps control and bidi characters out of the identity attributes too", () => {
    const hostile = HOSTILE_CODEPOINTS.join("");
    const socketPath = `/tmp/de${hostile}ck.sock`;
    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    const { container } = renderOverview({
      snapshot: {
        ...base,
        connection: { ...base.connection, socketPath },
        agents: [{ ...(first as AgentSession), daemonId: socketPath, tab: { kind: "mode", name: `re${hostile}view` } }],
      },
    });

    const values = identityAttributes(container).map((entry) => entry.value).join(" ~ ");
    for (const codepoint of HOSTILE_CODEPOINTS) {
      const name = `U+${codepoint.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0")}`;
      expect(values.includes(codepoint), `${name} reached a DOM identity attribute`).toBe(false);
    }
    expect(screen.getByTestId("daemon-group")).toHaveAttribute("data-daemon-id", "/tmp/deck.sock");
    expect(groupCard("mode", `re${hostile}view`)).toHaveAttribute("data-group-id", "review");
  });

  /**
   * Scenario: render two standalone agents whose display names are made
   * entirely of retained zero-width characters and differ only by one of them.
   * Both rows must name themselves visibly, and differently.
   */
  it("names an agent whose display name renders as nothing at all", () => {
    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    renderOverview({
      snapshot: {
        ...base,
        agents: [
          { ...(first as AgentSession), id: "7", displayName: "\u200b\u200d", tab: { kind: "dashboard" } },
          { ...(first as AgentSession), id: "8", displayName: "\u200b", tab: { kind: "dashboard" } },
        ],
      },
    });

    // Two blank identity cells used to be indistinguishable here, and the other
    // columns do not rescue them: CSS hides CLI and working directory below
    // 1180px and the tool columns below 680px. The fallback is deliberately not
    // a wider strip list — ZWJ and ZWNJ are load-bearing in emoji sequences and
    // in Persian, Arabic and Indic orthography.
    expect(rowNames(groupCard("standalone", "standalone"))).toEqual(["unnamed agent 7", "unnamed agent 8"]);
  });

  it("names a group whose title renders as nothing at all", () => {
    renderOverview({ snapshot: snapshotWithAgent({ tab: { kind: "orchestration", orchestrationId: "orc-x", name: "\u200b\ufeff", roleName: "writer", roleIndex: 0, isStartRole: true } }) });

    const card = groupCard("orchestration", "orc-x");
    expect(within(card).getByRole("heading")).toHaveTextContent("unnamed orchestration orc-x");
  });

  /**
   * Scenario: render one orchestration agent that reports no `orchestrationId`
   * alongside another whose EXPLICIT id is the exact string the old code minted
   * for the first. Two cards, not one.
   */
  it("renders an id-less orchestration as its own card when another claims its synthetic key", () => {
    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    const anonymous: AgentSession = { ...(first as AgentSession), id: "1", displayName: "no-id", tab: { kind: "orchestration", name: "alpha", roleName: "writer", roleIndex: 0, isStartRole: true } };
    // Both halves of this are knowable — a socket path and a daemon-minted
    // agent id — so a hostile orchestration can report it as its own id.
    const forged = `agent:${agentKey(anonymous)}`;
    renderOverview({
      snapshot: {
        ...base,
        agents: [
          anonymous,
          { ...(first as AgentSession), id: "2", displayName: "impostor", tab: { kind: "orchestration", orchestrationId: forged, name: "beta", roleName: "writer", roleIndex: 0, isStartRole: true } },
        ],
      },
    });

    expect(screen.getAllByRole("article")).toHaveLength(2);
    expect(rowNames(groupCard("orchestration", forged))).toEqual(["impostor"]);
    const own = screen.getByTestId(`overview-group-${anonymousOrchestrationKey(anonymous)}`);
    expect(rowNames(own)).toEqual(["no-id"]);
    // And it claims no daemon-side identity, so nothing advertises a drill-in
    // target the daemon has never heard of.
    expect(own).not.toHaveAttribute("data-group-id");
  });

  /**
   * The group heading names its table through `aria-labelledby`, and an IDREF
   * containing a space matches nothing at all — the association just stops,
   * with no error and nothing visibly wrong. A daemon-supplied mode name is
   * exactly where a space arrives.
   */
  it("keeps the group heading associated with its table when the mode name contains spaces", () => {
    renderOverview({ snapshot: snapshotWithAgent({ tab: { kind: "mode", name: "code review" } }) });

    const card = groupCard("mode", "code review");
    const table = within(card).getByRole("table", { name: "code review" });
    const labelledBy = table.getAttribute("aria-labelledby") ?? "";
    expect(labelledBy).not.toMatch(/\s/);
    expect(document.getElementById(labelledBy)).toHaveTextContent("code review");
    expect(card).toHaveAttribute("data-group-id", "code review");
  });

  it("renders both groups when a mode name collides with an orchestration id", () => {
    const base = createFixtureSnapshot("crowded");
    const [first] = base.agents;
    renderOverview({
      snapshot: {
        ...base,
        agents: [
          { ...(first as AgentSession), id: "1", displayName: "in-orchestration", tab: { kind: "orchestration", orchestrationId: "review", name: "review", roleName: "writer", roleIndex: 0, isStartRole: true } },
          { ...(first as AgentSession), id: "2", displayName: "in-mode", tab: { kind: "mode", name: "review" } },
        ],
      },
    });

    expect(rowNames(groupCard("orchestration", "review"))).toEqual(["in-orchestration"]);
    expect(rowNames(groupCard("mode", "review"))).toEqual(["in-mode"]);
    expect(screen.getAllByRole("article")).toHaveLength(2);
  });

  it("exposes real column headers so a screen reader can name the column a cell is in", () => {
    renderOverview();

    const prd = groupCard("orchestration", "orc-745");
    const table = within(prd).getByRole("table");
    expect(within(table).getAllByRole("columnheader").map((header) => header.textContent))
      .toEqual(["Status", "Agent", "State", "CLI", "Active tool", "Tools", "Working directory", "Last prompt"]);
    for (const row of rows(prd)) expect(within(row).getAllByRole("cell")).toHaveLength(8);
    // The visible legend is decoration for the shared grid and stays out of the
    // accessibility tree, so the columns are not announced twice.
    expect(document.querySelector(".overview-legend")).toHaveAttribute("aria-hidden", "true");
  });

  /**
   * Scenario: render one agent whose daemon reported a last user prompt, then
   * the same agent with none. The prompt cell prints what the daemon said, with
   * the full value on hover; with no prompt the cell is empty and carries no
   * hover text at all (PRD #745 M8).
   */
  it("prints the daemon's last user prompt, and nothing at all when there is none", () => {
    const { unmount } = renderOverview({
      snapshot: snapshotWithAgent({ lastUserPrompt: "Fix the flaky attach test and report back.", tab: { kind: "dashboard" } }),
    });

    const cell = document.querySelector(".overview-prompt");
    expect(cell).toHaveTextContent("Fix the flaky attach test and report back.");
    expect(cell).toHaveAttribute("title", "Fix the flaky attach test and report back.");
    unmount();

    renderOverview({ snapshot: snapshotWithAgent({ lastUserPrompt: undefined, tab: { kind: "dashboard" } }) });
    const blank = document.querySelector(".overview-prompt");
    // Blank, and no hover either — not a placeholder, and not a dash.
    expect(blank?.textContent).toBe("");
    expect(blank).not.toHaveAttribute("title");
  });

  /**
   * Scenario: render an agent for each write lease the daemon can report, then
   * one whose lease is the deck's `"unknown"` sentinel. The three reported
   * values print; the sentinel prints nothing, and in particular does not print
   * the word "unknown" on a screen that promises no placeholders.
   */
  it("prints a reported write lease and nothing for the absent one", () => {
    for (const lease of ["write", "read", "none"] as const) {
      const { unmount } = renderOverview({ snapshot: snapshotWithAgent({ writeLease: lease, tab: { kind: "dashboard" } }) });
      expect(document.querySelector(".overview-lease")).toHaveTextContent(lease);
      unmount();
    }

    const { container } = renderOverview({ snapshot: snapshotWithAgent({ writeLease: "unknown", tab: { kind: "dashboard" } }) });
    expect(document.querySelector(".overview-lease")).toBeNull();
    expect([container.textContent ?? "", ...titlesOf(container)].join(" ~ ")).not.toContain("unknown");
  });

  /**
   * Scenario: `toOverviewAgent` is the boundary that reverses the deck's
   * sentinels. A `"unknown"` lease becomes absent exactly as an `UNREPORTED`
   * cwd does, so no screen rendering from the honest projection can print
   * either one.
   */
  it("reverses the write-lease sentinel at the same boundary as the cwd one", () => {
    const [agent] = createFixtureSnapshot("crowded").agents;
    expect(toOverviewAgent({ ...(agent as AgentSession), writeLease: "unknown", cwd: UNREPORTED }))
      .toMatchObject({ writeLease: undefined, cwd: undefined });
    expect(toOverviewAgent({ ...(agent as AgentSession), writeLease: "read", cwd: "/tmp/project" }))
      .toMatchObject({ writeLease: "read", cwd: "/tmp/project" });
  });

  /**
   * Scenario: an orchestration whose roles work in three different directories,
   * but whose tab cwd the daemon states. The header states the daemon's value
   * rather than falling back to "no two members agree, print every row", and
   * the row that happens to match it stays blank.
   */
  it("states the orchestration's own directory in the header, in preference to a derived one", () => {
    const base = createFixtureSnapshot("crowded");
    const [seed] = base.agents;
    const inOrchestration = (id: string, cwd: string, roleIndex: number): AgentSession => ({
      ...(seed as AgentSession),
      id,
      cwd,
      displayName: `role-${roleIndex}`,
      tab: { kind: "orchestration", orchestrationId: "orc-1", name: "deck", roleName: `role-${roleIndex}`, roleIndex, isStartRole: false, cwd: "/work/deck" },
    });
    const agents = [inOrchestration("1", "/work/deck", 0), inOrchestration("2", "/work/other", 1), inOrchestration("3", "/work/third", 2)];

    const [group] = groupAgents(agents.map(toOverviewAgent));
    // No two members share a directory, so the derived answer is nothing — and
    // the stated one is what the header uses.
    expect(group?.commonCwd).toBeUndefined();
    expect(group?.orchestrationCwd).toBe("/work/deck");
    expect(hoistedCwdOf(group as NonNullable<typeof group>)).toBe("/work/deck");

    renderOverview({ snapshot: { ...base, agents } });
    const card = groupCard("orchestration", "orc-1");
    expect(card.querySelector(".overview-group-cwd")).toHaveTextContent("/work/deck");
    expect(card.querySelector(".overview-group-cwd")).toHaveAttribute("data-cwd-source", "orchestration");
    expect(rows(card).map((row) => row.querySelector(".overview-cwd")?.textContent))
      .toEqual(["", "/work/other", "/work/third"]);
  });

  /**
   * Scenario: a group with no stated orchestration cwd keeps the derived
   * behaviour exactly — the majority directory is hoisted and marked as the
   * shared one, not as something the daemon stated.
   */
  it("falls back to the shared directory when the daemon stated no orchestration cwd", () => {
    renderOverview();

    const standalone = groupCard("standalone", "standalone");
    const cwd = standalone.querySelector(".overview-group-cwd");
    expect(cwd).toHaveTextContent("~/code/dot-agent-deck");
    expect(cwd).toHaveAttribute("data-cwd-source", "shared");
  });

  /**
   * Scenario: a hostile prompt — every stripped codepoint, then far more text
   * than the budget allows. The rendered copy carries no control or bidi
   * character, is clamped to `DISPLAY_LIMITS.prompt` plus the elision marker,
   * and the row it sits in is exactly as tall as one with no prompt at all.
   */
  it("sanitises and bounds the last prompt, which is the most attacker-shaped string on the screen", () => {
    const hostile = `${HOSTILE_CODEPOINTS.join("")}${"p".repeat(DISPLAY_LIMITS.prompt * 4)}`;
    const { container } = renderOverview({ snapshot: snapshotWithAgent({ lastUserPrompt: hostile, tab: { kind: "dashboard" } }) });

    const cell = container.querySelector(".overview-prompt");
    const text = cell?.textContent ?? "";
    for (const codepoint of HOSTILE_CODEPOINTS) expect(text).not.toContain(codepoint);
    expect(Array.from(text).length).toBe(DISPLAY_LIMITS.prompt + 1);
    // And the budget is a bound in its own right, not merely "whatever the
    // constant happens to say": a prompt cell allowed to print several hundred
    // characters would dominate every row and every screenshot, while one much
    // shorter than the column can show would truncate a real first clause. The
    // exact number stays a design choice inside this range.
    expect(DISPLAY_LIMITS.prompt).toBeGreaterThanOrEqual(80);
    expect(DISPLAY_LIMITS.prompt).toBeLessThanOrEqual(DISPLAY_LIMITS.message);
    // The hover copy is bounded too, by the title budget rather than this one.
    expect(Array.from(cell?.getAttribute("title") ?? "").length).toBe(DISPLAY_LIMITS.title + 1);
    // One line, whatever the daemon sent: the cell never wraps, so no prompt
    // can push its row taller than its neighbours.
    expect(cell).toHaveClass("overview-prompt");
  });

  it("leaves the working directory blank when the daemon reported none", () => {
    // What `agentFromDto` produces for an agent whose `cwd` the daemon omitted.
    renderOverview({ snapshot: snapshotWithAgent({ cwd: UNREPORTED, tab: { kind: "dashboard" } }) });

    const cell = document.querySelector(".overview-cwd");
    // Blank, and no hover text either — a `title` is the other half of what a
    // reader sees, and there is nothing honest to put in it.
    expect(cell).toHaveTextContent("");
    expect(cell).not.toHaveAttribute("title");
    // Not a placeholder, and not a dash standing in for one.
    expect(cell?.textContent).toBe("");
  });

  it("shows nothing the daemon cannot report", () => {
    const snapshot = createFixtureSnapshot("crowded");
    // Crowded agents carry live mode's own placeholders in every field the
    // daemon does not have, so a leak onto the screen shows up as this string.
    expect(snapshot.agents[0]?.model).toBe(UNREPORTED);
    expect(snapshot.agents[0]?.worktree).toBe(UNREPORTED);
    // And one agent whose cwd is absent too: the `Pick<>` closes dishonest
    // field names but cannot close a sentinel inside an allowed field, so the
    // path the honesty claim was actually violable through is in this fleet.
    snapshot.agents = [...snapshot.agents, { ...(snapshot.agents[0] as AgentSession), id: "99", displayName: "no-cwd", cwd: UNREPORTED, tab: { kind: "dashboard" } }];

    const { container } = renderOverview({ snapshot });
    const text = [container.textContent ?? "", ...titlesOf(container)].join(" ~ ");
    expect(screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "99" })}`)).toBeVisible();
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

  /**
   * The header must not out-assert the body. The `disconnected` fixture keeps
   * the default four agents, and live mode does the same on a failed reconnect
   * — it replaces the connection and keeps the previous snapshot's fleet — so
   * counters derived from `snapshot.agents` printed `AGENTS 4 · GROUPS 1` over
   * a body correctly saying the fleet cannot be read.
   */
  it("counts nothing while the daemon is unreachable, however many agents the last snapshot held", () => {
    const snapshot = createFixtureSnapshot("disconnected");
    expect(snapshot.agents.length).toBeGreaterThan(0);

    renderOverview({ snapshot });

    expect(counterText()).toEqual(["—", "—", "—", "—", "—"]);
    expect(screen.getByTestId("overview-disconnected")).toBeVisible();
    // And the header's status pips, which say the same thing in words.
    expect(document.querySelector(".daemon-pips")).toBeNull();
  });

  it("counts nothing while the control channel is still opening", () => {
    const snapshot = createFixtureSnapshot("crowded");
    snapshot.connection = { status: "loading", socketPath: FIXTURE_DAEMON_ID, message: "Connecting to the daemon" };
    renderOverview({ snapshot });

    expect(counterText()).toEqual(["—", "—", "—", "—", "—"]);
    expect(screen.getByTestId("overview-loading")).toBeVisible();
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
    // This snapshot carries the default fleet, so the counters are exactly
    // where "cannot read them" would be contradicted by a number.
    expect(snapshot.agents.length).toBeGreaterThan(0);
    expect(counterText()).toEqual(["—", "—", "—", "—", "—"]);
  });

  /**
   * Issue #801. The overview is the screen a user lands on to see the fleet, so
   * a daemon it refuses for a stamp difference alone has to offer the same way
   * out the deck does — not a pointer to another screen.
   */
  it("offers Connect anyway on the overview when only the build stamps differ", async () => {
    const snapshot = createFixtureSnapshot("error");
    snapshot.connection = {
      ...snapshot.connection,
      daemonDetected: true,
      runningAgentCount: 9,
      message: "build mismatch: desktop is v0.38.0-50-gf118e99, daemon is v0.39.0. The daemon reports 9 live agents; stop them individually before replacing the daemon, or Connect anyway to keep this one.",
      buildStampMismatchOnly: true,
    };
    const runAction = vi.fn(async () => ({ ok: true }) as import("../types").DeckActionResult);
    const reconnect = vi.fn(async () => undefined);
    const deck = runtime({ mode: "live", snapshot, runAction, reconnect });
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} />);

    expect(screen.getByTestId("overview-incompatible")).toBeVisible();
    fireEvent.click(screen.getByTestId("overview-connect-anyway"));
    expect(runAction).not.toHaveBeenCalled();
    expect(screen.getByRole("alertdialog")).toHaveTextContent("The wire protocol matched on both sides");
    fireEvent.click(screen.getAllByRole("button", { name: "Connect anyway" }).at(-1)!);

    await waitFor(() => expect(runAction).toHaveBeenCalledWith({ type: "allow_build_mismatch" }));
    await waitFor(() => expect(reconnect).toHaveBeenCalled());
  });

  /** The same load-bearing negative as on the deck: the wire check is not negotiable. */
  it("never offers Connect anyway on the overview for a protocol mismatch", () => {
    const snapshot = createFixtureSnapshot("error");
    snapshot.connection = {
      ...snapshot.connection,
      daemonDetected: true,
      runningAgentCount: 3,
      message: "protocol mismatch: desktop expects 8, daemon reports 7",
      buildStampMismatchOnly: false,
    };
    render(<AgentOverview runtime={runtime({ mode: "live", snapshot })} onNavigate={vi.fn()} />);

    expect(screen.getByTestId("overview-incompatible")).toBeVisible();
    expect(screen.queryByTestId("overview-connect-anyway")).not.toBeInTheDocument();
  });

  /**
   * The caveat outlives the override here too. The overview has no banner of
   * its own — the daemon card's state line is where the connection message
   * lives, and it renders whatever the crate kept in it.
   */
  it("keeps the build-mismatch caveat on the daemon card after connecting anyway", () => {
    const snapshot = createFixtureSnapshot("crowded");
    snapshot.connection = {
      ...snapshot.connection,
      status: "connected",
      daemonDetected: true,
      runningAgentCount: 9,
      message: "build mismatch: desktop is v0.38.0-50-gf118e99, daemon is v0.39.0. Connected anyway for this session; protocol 8 matched on both sides.",
      buildStampMismatchOnly: true,
    };
    render(<AgentOverview runtime={runtime({ mode: "live", snapshot })} onNavigate={vi.fn()} />);

    expect(document.querySelector(".daemon-state")).toHaveTextContent("Connected anyway for this session");
    // Connected means the fleet is readable again, so it is listed.
    expect(rows(document.body).length).toBeGreaterThan(0);
    expect(screen.queryByTestId("overview-connect-anyway")).not.toBeInTheDocument();
  });

  it("reconnects on demand", () => {
    const deck = runtime();
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} />);

    fireEvent.click(screen.getByTestId("overview-refresh"));
    expect(deck.reconnect).toHaveBeenCalled();
  });

  /**
   * Scenario: render the overview against the crowded fifteen-agent fleet and
   * watch what it tells the bridge. Its header claims "no terminals attached",
   * and this is that claim as an instruction rather than as prose: declaring the
   * empty shown set is what detaches whatever the deck left warm on the way here
   * (PRD #745 M7). Rendering no terminal is NOT the same as attaching none —
   * mounting nothing was already true before M7 and the sockets stayed open.
   */
  it("declares an empty shown set so the screen holds no PTY", () => {
    const setShownTerminals = vi.fn(async () => undefined);
    renderOverview({ setShownTerminals });

    expect(terminalMounted).not.toHaveBeenCalled();
    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenCalledWith([]);
  });
});

describe("groupAgents", () => {
  const crowded = () => createFixtureSnapshot("crowded").agents.map(toOverviewAgent);

  /**
   * Scenario: group the crowded fleet fed in a deliberately reversed arrival
   * order and read the buckets back. The standalone bucket must come FIRST —
   * ahead of both orchestrations and the mode — mirroring the TUI, whose
   * dashboard tab is always the first tab; and it must land there whatever
   * order the agents arrived in.
   */
  it("puts the standalone bucket first however the agents arrive", () => {
    const groups = groupAgents([...crowded()].reverse());

    expect(groups.map((group) => group.kind)).toEqual(["standalone", "orchestration", "orchestration", "mode"]);
    expect(groups[0]?.id).toBe("standalone");
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

  /**
   * Scenario: group one id-less orchestration agent together with another whose
   * explicit `orchestrationId` is the synthetic key the old code minted for the
   * first. They must stay two groups.
   */
  it("keeps an id-less orchestration out of a group that names its synthetic key", () => {
    const [seed] = crowded();
    const anonymous: OverviewAgent = { ...seed, id: "1", displayName: "no-id", tab: { kind: "orchestration", name: "alpha", roleName: "writer", roleIndex: 0, isStartRole: true } };
    // The old fallback was `agent:${agentKey(agent)}`, with a comment claiming
    // it was "unique to itself". It was not: this string is a socket path and a
    // daemon-minted agent id, both knowable, and an orchestration reporting it
    // as its EXPLICIT id landed in the same map entry — one card, one title,
    // colliding role indexes and a misleading count.
    const groups = groupAgents([
      anonymous,
      { ...seed, id: "2", displayName: "impostor", tab: { kind: "orchestration", orchestrationId: `agent:${agentKey(anonymous)}`, name: "beta", roleName: "writer", roleIndex: 0, isStartRole: true } },
    ]);

    expect(groups).toHaveLength(2);
    expect(groups.map((group) => group.agents.map((agent) => agent.displayName))).toEqual([["no-id"], ["impostor"]]);
    expect(new Set(groups.map((group) => group.key)).size).toBe(2);
  });

  it("keys an id-less orchestration outside the space any explicit id can reach", () => {
    const [seed] = crowded();
    const anonymous: OverviewAgent = { ...seed, id: "1", tab: { kind: "orchestration", name: "alpha", roleName: "writer", roleIndex: 0, isStartRole: true } };
    const [group] = groupAgents([anonymous]);

    // The disjointness proof, rather than a sample of strings that happen not
    // to collide: an explicit key always begins `orchestration:` because
    // `encodeURIComponent` cannot produce that separator, and the anonymous key
    // never does.
    for (const id of ["orc-745", `agent:${agentKey(anonymous)}`, "orchestration-anonymous:x", ""]) {
      expect(groupKey("orchestration", id).startsWith("orchestration:")).toBe(true);
    }
    expect(group?.key).toBe(anonymousOrchestrationKey(anonymous));
    expect(group?.key.startsWith("orchestration:")).toBe(false);
    // And no daemon-side id is invented: there is none to carry.
    expect(group?.id).toBeUndefined();
  });

  /**
   * Orchestration ids, mode names and the standalone literal used to share one
   * key space, so a mode whose name equalled an orchestration id gave two
   * sibling cards the same React key and the same `data-testid`.
   */
  it("keeps a mode and an orchestration apart when their names collide", () => {
    const [seed] = crowded();
    const groups = groupAgents([
      { ...seed, id: "1", tab: { kind: "orchestration", orchestrationId: "review", name: "review", roleName: "writer", roleIndex: 0, isStartRole: true } },
      { ...seed, id: "2", tab: { kind: "mode", name: "review" } },
    ]);

    expect(groups.map((group) => group.id)).toEqual(["review", "review"]);
    expect(new Set(groups.map((group) => group.key)).size).toBe(2);
    expect(groups.map((group) => group.agents.length)).toEqual([1, 1]);
  });

  it("keeps a mode named `standalone` out of the standalone bucket", () => {
    const [seed] = crowded();
    const groups = groupAgents([
      { ...seed, id: "1", tab: { kind: "mode", name: "standalone" } },
      { ...seed, id: "2", tab: { kind: "dashboard" } },
    ]);

    expect(new Set(groups.map((group) => group.key)).size).toBe(2);
    expect(groups.map((group) => group.kind)).toEqual(["standalone", "mode"]);
  });

  it("produces a key that is legal as an HTML id whatever the daemon named the group", () => {
    // No whitespace: `aria-labelledby` is a space-separated token list, so a
    // single space in a raw name silently splits the reference in two.
    for (const name of ["code review", "a\tb", "50%", "why?", "#hash"]) {
      expect(groupKey("mode", name)).not.toMatch(/\s/);
    }
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

  /**
   * The live-mode path through both seams, which is now the product (PRD #745
   * M7). These two tests are the inverted survivors of the fixture gate: they
   * asserted the screen was unreachable in live mode, because attach was
   * snapshot-driven and a live overview would have held one socket and one
   * scrollback replay per agent behind a screen claiming it held none. Attach
   * is demand-driven now, so the reason is gone and so is the gate — and the
   * same two seams still need covering, because either one left alone would
   * leave the screen half-reachable.
   */
  it("offers the overview in live mode", () => {
    render(<DeckShell runtime={runtime({ mode: "live", snapshot: createFixtureSnapshot("connected") })} />);

    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expect(screen.getByTestId("open-overview")).toBeVisible();
    expect(screen.getByRole("button", { name: "Overview" })).toBeVisible();

    terminalMounted.mockClear();
    fireEvent.click(screen.getByTestId("open-overview"));

    expect(screen.getByTestId("daemon-group")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
    expect(terminalMounted).not.toHaveBeenCalled();
  });

  /**
   * The other seam: a live session that already holds an `overview` view state
   * — restored, or navigated to before a re-render — renders the overview
   * rather than falling back to the deck.
   */
  it("renders the overview when a live session is already holding an overview view state", () => {
    render(<DeckShell runtime={runtime({ mode: "live", snapshot: createFixtureSnapshot("connected") })} initialView={{ kind: "overview" }} />);

    expect(screen.getByTestId("daemon-group")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
    expect(terminalMounted).not.toHaveBeenCalled();
  });

  it("still honours an overview view state in fixture mode", () => {
    render(<DeckShell runtime={runtime()} initialView={{ kind: "overview" }} />);

    expect(screen.getByTestId("daemon-group")).toBeVisible();
    expect(terminalMounted).not.toHaveBeenCalled();
  });
});
