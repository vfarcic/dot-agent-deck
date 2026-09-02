import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
/*
  The shipped stylesheet, loaded so the assertions below can read COMPUTED
  declarations rather than only class names. Vitest's `css: true` hands it to
  JSDOM, which applies the cascade — so a cell's `white-space` here is the one
  `styles.css` really gives it, and deleting the rule fails the test. JSDOM
  still performs no LAYOUT: nothing in this file measures a height, and no
  assertion pretends to.
*/
import "../styles.css";
/*
  The same stylesheet as SOURCE, for the one assertion JSDOM cannot make: it
  does not evaluate media queries at all, so a `display: none` inside one is
  invisible to `getComputedStyle` and the only way to see it is to read the
  rules.
*/
import stylesheetSource from "../styles.css?raw";
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
import { agentDomKey, agentKey, AgentOverview, ALL_OVERVIEW_COLUMNS, OVERVIEW_CLOCK_TICK_MS, anonymousOrchestrationKey, DEFAULT_OVERVIEW_COLUMNS, gridTemplateFor, groupAgents, groupKey, hoistedCwdOf, orderedColumns, OVERVIEW_COLUMNS_STORAGE_KEY, PERMANENT_COLUMN, readStoredColumns, type OverviewAgent, type OverviewColumnId, type OverviewGroupKind, toOverviewAgent } from "./AgentOverview";

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

/**
 * The overview with EVERY column on screen, which is what most of this file is
 * about: what a cell says, not which cells are chosen. The choice is seeded
 * through the same `localStorage` key a user's own click lands in, so this is a
 * real state of the screen rather than a test-only path — and it keeps these
 * tests honest about the one thing that changed under them in M12, which is the
 * DEFAULT set and not any cell's content.
 *
 * Tests about the defaults, the picker and persistence use
 * `renderOverviewWithStoredColumns` instead.
 */
function renderOverview(overrides: Partial<DeckRuntimeState> = {}) {
  window.localStorage.setItem(OVERVIEW_COLUMNS_STORAGE_KEY, JSON.stringify({ columns: ALL_OVERVIEW_COLUMNS }));
  return render(<AgentOverview runtime={runtime(overrides)} onNavigate={vi.fn()} />);
}

/**
 * The overview against a literal stored value — `undefined` for a first visit
 * with nothing stored at all, and otherwise exactly the bytes an older build,
 * or something else entirely, left in that key.
 */
function renderOverviewWithStoredColumns(stored: string | undefined, overrides: Partial<DeckRuntimeState> = {}) {
  if (stored === undefined) window.localStorage.removeItem(OVERVIEW_COLUMNS_STORAGE_KEY);
  else window.localStorage.setItem(OVERVIEW_COLUMNS_STORAGE_KEY, stored);
  return render(<AgentOverview runtime={runtime(overrides)} onNavigate={vi.fn()} />);
}

/** The legend's labels, which is the columns as a reader sees them named. */
function legendLabels(): (string | null)[] {
  return Array.from(document.querySelectorAll(".overview-legend > span")).map((span) => span.textContent);
}

/** The column headers of one group card, which is what a screen reader gets. */
function headerLabels(scope: HTMLElement): (string | null)[] {
  return within(scope).getAllByRole("columnheader").map((header) => header.textContent);
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
    expect(coder).toHaveTextContent("claude");
    expect(coder).toHaveTextContent("edit");
    expect(coder).toHaveTextContent("desktop/src/components/AgentOverview.tsx");
    expect(coder).toHaveTextContent("132");

    // An agent with no active tool says so rather than borrowing a placeholder.
    const docs = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "5" })}`);
    expect(docs).toHaveTextContent("no active tool");
  });

  /**
   * Scenario: read the CLI column down the whole fleet. Every cell names a
   * BINARY somebody could type. It used to render the serialised agent-type
   * enum, so Claude Code read `claude_code` and OpenCode read `open_code`,
   * with `codex` right only by coincidence — the name now comes from the agent
   * registry, which is where the deck already keeps each agent's command
   * (PRD #745).
   */
  it("names the binary each agent runs, never the enum the wire keys it by", () => {
    const { container } = renderOverview();

    const cells = Array.from(container.querySelectorAll(".overview-cli")).map((cell) => cell.textContent);
    expect(new Set(cells)).toEqual(new Set(["claude", "opencode", "codex", "pi"]));
    for (const cell of cells) expect(cell).not.toContain("_");

    // The hover is the full value and nothing else: the column header already
    // says what it is, and a sentence restating that is the screen describing
    // itself.
    const coder = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "2" })}`);
    expect(coder.querySelector(".overview-cli")).toHaveAttribute("title", "claude");
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

  /**
   * Scenario: look at the daemon card's header. It names the daemon and shows
   * no socket filename at all — the shortened label that used to sit there read
   * `dot-agent-deck-attach-501.sock` on a default socket, so it carried the very
   * uid it existed to keep out of a screenshot and told the reader nothing they
   * could act on. The full path is one hover away, and the raw path is still the
   * identity key (PRD #745).
   */
  it("shows no socket filename in the daemon header and keeps the full path on hover", () => {
    const { container } = renderOverview();

    const header = screen.getByTestId("daemon-group");
    expect(header).toHaveTextContent("Local daemon");
    // Not shortened, not abbreviated — absent. No segment of the socket path is
    // on screen, and neither is the uid the old label leaked.
    expect(container.textContent ?? "").not.toContain(".sock");
    expect(container.textContent ?? "").not.toContain("/tmp/");
    expect(header.querySelector(".daemon-identity code")).toBeNull();

    // Diagnostic rather than decorative, so hover keeps it in full.
    expect(screen.getByTestId("daemon-identity")).toHaveAttribute("title", FIXTURE_DAEMON_ID);
    // The identity key is still the raw socket path.
    expect(header).toHaveAttribute("data-daemon-id", FIXTURE_DAEMON_ID);
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
    expect(headerLabels(prd))
      .toEqual(["Status", "Agent", "Last activity", "Uptime", "CLI", "Active tool", "Tools", "Working directory", "Last prompt"]);
    /*
      One cell per chosen column, and the count is the point rather than a
      formality: the legend, the `<thead>` and every row are generated from ONE
      list, so a row out of step with it would put a header over the wrong cell
      on the screen whose whole job is being readable at a glance. The legend's
      own span count is checked with it, since it shares the grid template.
    */
    for (const row of rows(prd)) expect(within(row).getAllByRole("cell")).toHaveLength(ALL_OVERVIEW_COLUMNS.length);
    expect(document.querySelectorAll(".overview-legend > span")).toHaveLength(ALL_OVERVIEW_COLUMNS.length);
    // The visible legend is decoration for the shared grid and stays out of the
    // accessibility tree, so the columns are not announced twice.
    expect(document.querySelector(".overview-legend")).toHaveAttribute("aria-hidden", "true");
  });

  /**
   * Scenario: open the overview for the first time, with nothing stored. Four
   * columns are on screen — the agent's name, its status, where it is working
   * and how long it has been running — and the five the daemon can also fill
   * are absent until asked for (PRD #745 M12).
   */
  it("opens on the four default columns and nothing else", () => {
    renderOverviewWithStoredColumns(undefined);

    expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "WORKING DIRECTORY"]);
    const prd = groupCard("orchestration", "orc-745");
    expect(headerLabels(prd)).toEqual(["Status", "Agent", "Uptime", "Working directory"]);
    for (const row of rows(prd)) expect(within(row).getAllByRole("cell")).toHaveLength(4);
    // The five that exist and were not chosen render no cell at all — not an
    // empty one, which would still occupy a grid track.
    expect(document.querySelector(".overview-activity")).toBeNull();
    expect(document.querySelector(".overview-cli")).toBeNull();
    expect(document.querySelector(".overview-tool")).toBeNull();
    expect(document.querySelector(".overview-tool-count")).toBeNull();
    expect(document.querySelector(".overview-prompt")).toBeNull();
  });

  /**
   * Scenario: with the defaults on screen, open the Columns menu and tick
   * `Last prompt`, then untick `Working directory`. Each lands immediately, and
   * the column appears in the screen's own order rather than at the end of the
   * click sequence (PRD #745 M12).
   */
  it("adds and removes a column from the picker, without leaving the overview", () => {
    renderOverviewWithStoredColumns(undefined);

    expect(screen.queryByTestId("overview-columns-menu")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    expect(screen.getByTestId("overview-columns-toggle")).toHaveAttribute("aria-expanded", "true");

    fireEvent.click(screen.getByTestId("overview-column-lastUserPrompt"));
    expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "WORKING DIRECTORY", "LAST PROMPT"]);

    fireEvent.click(screen.getByTestId("overview-column-cwd"));
    expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "LAST PROMPT"]);
    expect(document.querySelector(".overview-cwd")).toBeNull();

    // The deck is still one click away — the picker never took over the screen.
    expect(screen.getByTestId("overview-open-deck")).toBeVisible();
  });

  /**
   * Scenario: open the Columns menu, click a checkbox inside it (the menu
   * stays), then press the pointer down somewhere else on the screen — the menu
   * closes. `Escape` used to be the only way out, so a menu opened by accident
   * had to be dismissed by keyboard (PRD #745).
   */
  it("closes the column picker on an outside pointerdown and not on one inside it", () => {
    renderOverviewWithStoredColumns(undefined);
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    expect(screen.getByTestId("overview-columns-menu")).toBeVisible();

    // Inside the menu: choosing a column must not dismiss the menu you are
    // choosing from.
    fireEvent.pointerDown(screen.getByTestId("overview-column-cli"));
    fireEvent.click(screen.getByTestId("overview-column-cli"));
    expect(screen.getByTestId("overview-columns-menu")).toBeVisible();
    expect(legendLabels()).toContain("CLI");

    fireEvent.pointerDown(screen.getByTestId("overview-refresh"));
    expect(screen.queryByTestId("overview-columns-menu")).not.toBeInTheDocument();
    expect(screen.getByTestId("overview-columns-toggle")).toHaveAttribute("aria-expanded", "false");
  });

  /**
   * Scenario: with the menu open, click the Columns button again. It closes and
   * stays closed. The trigger sits INSIDE the dismissal boundary on purpose —
   * outside it, its own pointer-down would close the menu and its click would
   * toggle it straight back open, so the button would look broken (PRD #745).
   */
  it("closes the picker when its own trigger is clicked, without reopening it", () => {
    renderOverviewWithStoredColumns(undefined);
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    expect(screen.getByTestId("overview-columns-menu")).toBeVisible();

    fireEvent.pointerDown(screen.getByTestId("overview-columns-toggle"));
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    expect(screen.queryByTestId("overview-columns-menu")).not.toBeInTheDocument();
  });

  /**
   * Scenario: untick your way down to almost nothing, then click Restore
   * defaults. The four the screen opens on come back, and they are remembered
   * exactly as any other change is — without it, the only way back was
   * remembering which four they were (PRD #745).
   */
  it("restores the default columns from the picker and remembers them", () => {
    renderOverviewWithStoredColumns(JSON.stringify({ columns: ["displayName", "lastUserPrompt", "toolCount"] }));
    expect(legendLabels()).toEqual(["AGENT", "TOOLS", "LAST PROMPT"]);

    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    fireEvent.click(screen.getByTestId("overview-columns-reset"));

    expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "WORKING DIRECTORY"]);
    expect(JSON.parse(window.localStorage.getItem(OVERVIEW_COLUMNS_STORAGE_KEY) ?? "null"))
      .toEqual({ columns: [...DEFAULT_OVERVIEW_COLUMNS] });
    // The menu stays open, so the ticks a reader just changed are visible.
    expect(screen.getByTestId("overview-columns-menu")).toBeVisible();
  });

  /**
   * Scenario: open the picker and try to remove the agent's name. Its checkbox
   * is checked and disabled, so there is nothing to click — a row with no name
   * is not a shorter row, it is an anonymous one, on the screen whose whole job
   * is telling agents apart (PRD #745 M12).
   */
  it("will not let the name column be removed", () => {
    renderOverviewWithStoredColumns(undefined);
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));

    const name = screen.getByTestId(`overview-column-${PERMANENT_COLUMN}`);
    expect(name).toBeChecked();
    expect(name).toBeDisabled();

    fireEvent.click(name);
    expect(legendLabels()).toContain("AGENT");
    // And it comes back whatever a stored value asks for, which is the half a
    // disabled checkbox cannot enforce.
    expect(orderedColumns([])).toEqual([PERMANENT_COLUMN]);
    expect(readStoredColumns(JSON.stringify({ columns: ["cli"] }))).toEqual(["displayName", "cli"]);
  });

  /**
   * Scenario: choose a column, leave the screen, and come back. The choice is
   * still there — it is written to a mode-scoped `localStorage` key, so a
   * fixture visit and a live session never share one (PRD #745 M12).
   */
  it("remembers the chosen columns across a remount, under a mode-scoped key", () => {
    const { unmount } = renderOverviewWithStoredColumns(undefined);
    fireEvent.click(screen.getByTestId("overview-columns-toggle"));
    fireEvent.click(screen.getByTestId("overview-column-cli"));
    expect(legendLabels()).toContain("CLI");
    unmount();

    expect(OVERVIEW_COLUMNS_STORAGE_KEY).toBe("dot-agent-deck.desktop.overview-columns.v1.fixture");
    expect(JSON.parse(window.localStorage.getItem(OVERVIEW_COLUMNS_STORAGE_KEY) ?? "null"))
      .toEqual({ columns: ["status", "displayName", "spawnedAtMs", "cli", "cwd"] });

    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "CLI", "WORKING DIRECTORY"]);
  });

  /**
   * Scenario: start with each of the ways a stored value can be unusable —
   * unparseable bytes, a value of the wrong shape, and one naming only columns
   * that no longer exist — and confirm the screen opens on the defaults every
   * time rather than throwing or rendering a dead track. This is the case that
   * breaks silently a release later, when an older build's value meets a newer
   * column set (PRD #745 M12).
   */
  it("falls back to the defaults for a stored value it cannot use", () => {
    for (const stored of ["{not json", "null", '"columns"', "42", "{}", '{"columns":"cli"}', '{"columns":[]}', '{"columns":["model","cost","tokens"]}']) {
      const { unmount } = renderOverviewWithStoredColumns(stored);
      expect(legendLabels()).toEqual(["STATUS", "AGENT", "UPTIME", "WORKING DIRECTORY"]);
      unmount();
    }
    // And the normalised value is written back, so the next visit reads a
    // stored value this build understands rather than the same rubbish again.
    renderOverviewWithStoredColumns("{not json");
    expect(JSON.parse(window.localStorage.getItem(OVERVIEW_COLUMNS_STORAGE_KEY) ?? "null"))
      .toEqual({ columns: [...DEFAULT_OVERVIEW_COLUMNS] });
  });

  /**
   * Scenario: a stored value naming one real column and one that no longer
   * exists. The real one survives and the stale one is DROPPED — carried
   * through it would be a `<th>` with no cell under it and one dead grid track
   * down every card, which reads as a layout bug and is really a migration one
   * (PRD #745 M12).
   */
  it("drops a stored column that no longer exists and keeps the rest", () => {
    renderOverviewWithStoredColumns(JSON.stringify({ columns: ["cli", "contextPercent", "lastUserPrompt"] }));

    expect(legendLabels()).toEqual(["AGENT", "CLI", "LAST PROMPT"]);
    const prd = groupCard("orchestration", "orc-745");
    expect(headerLabels(prd)).toEqual(["Agent", "CLI", "Last prompt"]);
    for (const row of rows(prd)) expect(within(row).getAllByRole("cell")).toHaveLength(3);
  });

  /**
   * Scenario: check the grid template the chosen columns produce. Every group
   * card shares ONE template, published as a custom property on the single
   * scroll region that holds the legend and all the cards — which is what keeps
   * rows lining up across card boundaries when the table is wider than the
   * window (PRD #745 M12).
   */
  it("generates one grid template from the selection and scrolls the whole table region", () => {
    renderOverviewWithStoredColumns(JSON.stringify({ columns: ["displayName", "cli", "toolCount"] }));

    const region = screen.getByTestId("overview-table-region");
    const track = region.querySelector(".overview-table-track") as HTMLElement;
    expect(track.style.getPropertyValue("--overview-grid")).toBe("minmax(150px, 1.3fr) 78px 40px");
    expect(gridTemplateFor(["displayName", "cli", "toolCount"])).toBe("minmax(150px, 1.3fr) 78px 40px");

    // The scroll is on the region, not on a card: per-card scrolling would let
    // two cards sit at different offsets and the columns would desynchronise.
    expect(getComputedStyle(region).overflowX).toBe("auto");
    expect(getComputedStyle(track).minWidth).toBe("min-content");
    for (const card of screen.getAllByRole("article")) {
      expect(track).toContainElement(card);
      expect(getComputedStyle(card).overflowX).not.toBe("auto");
    }
    // Every flexible track carries a fixed px minimum rather than `minmax(0,
    // …)`, which is what gives the grid a min-content width to scroll past.
    expect(gridTemplateFor([...ALL_OVERVIEW_COLUMNS])).not.toContain("minmax(0");
  });

  /**
   * Scenario: read the shipped stylesheet and confirm no rule hides an overview
   * column by viewport. Two media queries used to drop three columns below
   * 1180px and three more below 680px, by `nth-child` index. Once the operator
   * picks the columns, hiding one by window width fights them silently — and
   * the index-sensitivity made adding a column a five-rule renumbering
   * (PRD #745 M12).
   *
   * Read from the file rather than from JSDOM's cascade on purpose: JSDOM does
   * not evaluate media queries at all, so a `display: none` inside one is
   * invisible to `getComputedStyle` and this is the only place it can be seen.
   */
  it("hides no column by viewport any more", () => {
    const stylesheet = stylesheetSource;

    // No `nth-child` anywhere: the index-sensitivity is what made adding a
    // column a five-rule renumbering, and it is gone with the templates.
    expect(stylesheet).not.toMatch(/\.overview-\w+[^{]*:nth-child/);
    // And no media query mentions a column at all — not to hide one, not to
    // re-template the grid. What is left in one is padding and the decorative
    // status pips, neither of which is a column.
    for (const block of stylesheet.match(/@media[^{]*\{[\s\S]*?\n\}/g) ?? []) {
      for (const selector of [".overview-legend", ".overview-row", ".overview-state", ".overview-agent-name", ".overview-activity", ".overview-uptime", ".overview-cli", ".overview-tool", ".overview-tool-count", ".overview-cwd", ".overview-prompt"]) {
        expect(block).not.toContain(selector);
      }
    }
  });

  /**
   * Scenario: render one agent the daemon last saw working two hours ago, then
   * the same agent with no reported activity time at all. The cell prints one
   * relative unit with the exact UTC instant on hover; with nothing reported it
   * is empty and carries no hover text — not a dash, not a placeholder, the
   * same rule every other honest column follows (PRD #745 M9).
   */
  it("prints how long ago the daemon last saw the agent, and nothing at all when it reported no time", () => {
    const twoHours = Date.now() - 2 * 60 * 60_000;
    const { unmount } = renderOverview({
      snapshot: snapshotWithAgent({ lastActivityMs: twoHours, tab: { kind: "dashboard" } }),
    });

    const cell = document.querySelector(".overview-activity");
    expect(cell).toHaveTextContent("2h ago");
    expect(cell?.getAttribute("title")).toBe(`Last activity reported by the daemon: ${new Date(twoHours).toISOString()}`);
    unmount();

    // The RESTARTED-daemon case, and the one that made this field shippable
    // where session duration was not: the daemon persists no session state, so
    // it reports no activity time rather than resetting every agent to "just
    // now". Blank says "I do not know", which is what is true.
    renderOverview({ snapshot: snapshotWithAgent({ lastActivityMs: undefined, tab: { kind: "dashboard" } }) });
    const blank = document.querySelector(".overview-activity");
    expect(blank?.textContent).toBe("");
    expect(blank).not.toHaveAttribute("title");
  });

  /**
   * Scenario: render one agent whose reported instant is a second in the future
   * and one whose instant is an hour in the future. The first is ordinary clock
   * skew between the hook process that stamped the event and the webview, and
   * reads "just now"; the second is beyond anything skew explains, so the cell
   * renders NOTHING rather than a negative "ago" or a fabricated "just now"
   * (PRD #745 M9).
   */
  it("reads a slightly-future instant as just now and renders nothing for one the clock cannot explain", () => {
    const { unmount } = renderOverview({
      snapshot: snapshotWithAgent({ lastActivityMs: Date.now() + 1_000, tab: { kind: "dashboard" } }),
    });
    expect(document.querySelector(".overview-activity")).toHaveTextContent("just now");
    unmount();

    renderOverview({ snapshot: snapshotWithAgent({ lastActivityMs: Date.now() + 60 * 60_000, tab: { kind: "dashboard" } }) });
    const refused = document.querySelector(".overview-activity");
    // No "-60m ago", and no "just now" either: the daemon does not clamp this
    // value (it is the ordering evidence `supersedes_generation` weighs), so
    // the render seam declines to relativise it at all.
    expect(refused?.textContent).toBe("");
    expect(refused).not.toHaveAttribute("title");
  });

  /**
   * Scenario: render one agent the daemon spawned three hours ago, then the
   * same agent with no reported spawn time at all. The uptime cell prints one
   * relative unit with NO "ago" — it names a span still running, not a moment
   * past — and carries the exact UTC spawn instant on hover; with nothing
   * reported it is empty and carries no hover text (PRD #745 M11).
   */
  it("prints how long the daemon has had the agent running, and nothing at all when it reported no spawn time", () => {
    const threeHours = Date.now() - 3 * 60 * 60_000;
    const { unmount } = renderOverview({
      snapshot: snapshotWithAgent({ spawnedAtMs: threeHours, tab: { kind: "dashboard" } }),
    });

    const cell = document.querySelector(".overview-uptime");
    expect(cell).toHaveTextContent("3h");
    expect(cell?.textContent).not.toContain("ago");
    expect(cell?.getAttribute("title")).toBe(`Spawned by the daemon at: ${new Date(threeHours).toISOString()}`);
    unmount();

    // The case a daemon that did not spawn the agent produces — an id-only
    // `ListAgents` reply, or a peer predating the field. Blank says "I do not
    // know", which is what is true; there is no `Date.now()` fallback anywhere
    // on this path, which is the failure the PRD's original duration rejection
    // was about.
    renderOverview({ snapshot: snapshotWithAgent({ spawnedAtMs: undefined, tab: { kind: "dashboard" } }) });
    const blank = document.querySelector(".overview-uptime");
    expect(blank?.textContent).toBe("");
    expect(blank).not.toHaveAttribute("title");
  });

  /**
   * Scenario: render one agent whose spawn instant is a second in the future
   * and one whose spawn instant is an hour in the future. The uptime column
   * obeys the SAME clock-skew rule as Last activity beside it — ordinary skew
   * reads as the sub-minute bucket, and anything beyond it renders nothing
   * rather than a negative duration (PRD #745 M11).
   */
  it("applies the same clock-skew rule to uptime as to last activity", () => {
    const { unmount } = renderOverview({
      snapshot: snapshotWithAgent({ spawnedAtMs: Date.now() + 1_000, tab: { kind: "dashboard" } }),
    });
    expect(document.querySelector(".overview-uptime")).toHaveTextContent("<1m");
    unmount();

    renderOverview({ snapshot: snapshotWithAgent({ spawnedAtMs: Date.now() + 60 * 60_000, tab: { kind: "dashboard" } }) });
    const refused = document.querySelector(".overview-uptime");
    expect(refused?.textContent).toBe("");
    expect(refused).not.toHaveAttribute("title");
  });

  /**
   * Scenario: render one agent the daemon spawned two hours ago that has never
   * reported any activity. Uptime prints; Last activity stays blank. This is
   * the case that decided the field's SOURCE — a session exists only once a
   * hook event has arrived, so `SessionState.started_at` has nothing to say
   * about exactly the agent whose uptime a reader most wants, while the daemon
   * knows perfectly well when it forked the process (PRD #745 M11).
   */
  it("reports uptime for an agent that has never emitted an event", () => {
    renderOverview({
      snapshot: snapshotWithAgent({
        spawnedAtMs: Date.now() - 2 * 60 * 60_000,
        lastActivityMs: undefined,
        tab: { kind: "dashboard" },
      }),
    });

    expect(document.querySelector(".overview-uptime")).toHaveTextContent("2h");
    expect(document.querySelector(".overview-activity")?.textContent).toBe("");
  });

  /**
   * Scenario: mount the overview with one agent seen 30 seconds ago, then let
   * five minutes pass with NO new snapshot — no daemon event, no reconnect,
   * nothing. Both time cells must have moved on their own: `just now` becomes
   * `5m ago`, and `<1m` becomes `5m` (PRD #745 M12).
   *
   * This is the freeze bug. Snapshots are event-driven off the daemon watch
   * stream and nothing polls, so an agent emitting nothing produced no
   * re-render at all and both columns stopped. It failed exactly backwards —
   * any OTHER agent's event repaints everything, so a busy fleet masked it and
   * the idle case, which is when "quiet for two hours" is the most valuable
   * thing on screen, is precisely when it stopped updating.
   */
  it("keeps the relative times counting while no snapshot arrives", () => {
    vi.useFakeTimers();
    try {
      const start = new Date("2026-09-02T09:00:00.000Z").getTime();
      vi.setSystemTime(start);
      renderOverview({
        snapshot: snapshotWithAgent({ lastActivityMs: start - 30_000, spawnedAtMs: start - 30_000, tab: { kind: "dashboard" } }),
      });

      expect(document.querySelector(".overview-activity")).toHaveTextContent("just now");
      expect(document.querySelector(".overview-uptime")).toHaveTextContent("<1m");

      act(() => void vi.advanceTimersByTime(5 * 60_000));

      expect(document.querySelector(".overview-activity")).toHaveTextContent("5m ago");
      expect(document.querySelector(".overview-uptime")).toHaveTextContent("5m");
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * Scenario: mount the overview, let it tick, then leave the screen. The
   * interval must be gone — a ticker that outlives its component keeps
   * repainting a tree nobody is looking at, for as long as the app runs.
   */
  it("ticks once every ten seconds and stops when the screen is left", () => {
    vi.useFakeTimers();
    const started = vi.spyOn(window, "setInterval");
    const stopped = vi.spyOn(window, "clearInterval");
    try {
      vi.setSystemTime(new Date("2026-09-02T09:00:00.000Z").getTime());
      const { unmount } = renderOverview({ snapshot: snapshotWithAgent({ tab: { kind: "dashboard" } }) });

      // One interval for the whole screen, at the cadence the columns need —
      // both round to the minute above their sub-minute bucket, so anything
      // faster is repaints that change no label.
      expect(started).toHaveBeenCalledTimes(1);
      expect(started).toHaveBeenCalledWith(expect.any(Function), OVERVIEW_CLOCK_TICK_MS);

      unmount();

      // A ticker that outlives its component keeps repainting a tree nobody is
      // looking at, for as long as the app runs.
      expect(stopped).toHaveBeenCalledWith(started.mock.results[0]?.value);
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * Scenario: mount the overview, hide the document, let 45 minutes pass, then
   * bring it back. While hidden nothing ticks — a backgrounded window
   * repainting six times a minute forever is pure waste — so the cell is
   * knowably stale. Becoming visible re-reads the clock IMMEDIATELY rather than
   * waiting out an interval, so the first thing the user sees is the true time
   * rather than the one from before they looked away (PRD #745 M12).
   */
  it("pauses while the document is hidden and catches up the moment it comes back", () => {
    vi.useFakeTimers();
    const started = vi.spyOn(window, "setInterval");
    const stopped = vi.spyOn(window, "clearInterval");
    const visibility = (state: DocumentVisibilityState) => {
      Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
      act(() => void document.dispatchEvent(new Event("visibilitychange")));
    };
    try {
      const start = new Date("2026-09-02T09:00:00.000Z").getTime();
      vi.setSystemTime(start);
      renderOverview({ snapshot: snapshotWithAgent({ lastActivityMs: start, tab: { kind: "dashboard" } }) });
      expect(started).toHaveBeenCalledTimes(1);

      visibility("hidden");
      expect(stopped).toHaveBeenCalledWith(started.mock.results[0]?.value);

      act(() => void vi.advanceTimersByTime(45 * 60_000));
      // Still the pre-hide reading: nothing ticked, which is the point.
      expect(document.querySelector(".overview-activity")).toHaveTextContent("just now");

      visibility("visible");
      expect(document.querySelector(".overview-activity")).toHaveTextContent("45m ago");
      expect(started).toHaveBeenCalledTimes(2);
    } finally {
      // @ts-expect-error — removing the own property restores the prototype getter.
      delete document.visibilityState;
      vi.useRealTimers();
    }
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
   * Scenario: `toOverviewAgent` is the boundary that reverses the deck's one
   * remaining sentinel. A `"unknown"` lease becomes absent — no daemon value
   * spells it, so the reversal can only ever remove a placeholder — while an
   * absent cwd arrives absent and needs no reversal at all.
   */
  it("reverses the write-lease sentinel, which is now the only one this boundary reverses", () => {
    const [agent] = createFixtureSnapshot("crowded").agents;
    expect(toOverviewAgent({ ...(agent as AgentSession), writeLease: "unknown", cwd: undefined }))
      .toMatchObject({ writeLease: undefined, cwd: undefined });
    expect(toOverviewAgent({ ...(agent as AgentSession), writeLease: "read", cwd: "/tmp/project" }))
      .toMatchObject({ writeLease: "read", cwd: "/tmp/project" });
  });

  /**
   * Scenario: the daemon reports a working directory whose name is the deck's
   * own stand-in word. `src/agent_pty.rs` accepts any non-empty, bounded,
   * control-free cwd, so that is a real directory and not an absence — and this
   * boundary, which used to reverse the word into `undefined`, now carries it
   * through to a cell with the path in it and a hover to match.
   */
  it("keeps a reported working directory that happens to spell the deck's stand-in word", () => {
    expect(toOverviewAgent({ ...(createFixtureSnapshot("crowded").agents[0] as AgentSession), cwd: UNREPORTED }))
      .toMatchObject({ cwd: UNREPORTED });

    renderOverview({ snapshot: snapshotWithAgent({ cwd: UNREPORTED, tab: { kind: "dashboard" } }) });

    const cell = document.querySelector(".overview-cwd");
    expect(cell).toHaveTextContent(UNREPORTED);
    expect(cell).toHaveAttribute("title", UNREPORTED);
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
   * and the cell carries the class whose shipped rule pins it to one line.
   *
   * That last assertion is over the cell's COMPUTED DECLARATIONS, not over its
   * geometry, and the distinction is the point: JSDOM performs no layout, so
   * nothing here can measure a row's height. Asserting only that the cell
   * carries `overview-prompt` would have passed with the clipping rule deleted
   * — a test claiming to see something it cannot — so the rule that makes the
   * class mean anything is asserted too.
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
    // can push its row taller than its neighbours. Both halves are needed —
    // the class on the cell, AND the rule that makes the class mean anything.
    expect(cell).toHaveClass("overview-prompt");
    const computed = getComputedStyle(cell as HTMLElement);
    expect(computed.whiteSpace).toBe("nowrap");
    expect(computed.overflow).toBe("hidden");
    expect(computed.textOverflow).toBe("ellipsis");
  });

  it("leaves the working directory blank when the daemon reported none", () => {
    // What `agentFromDto` produces for an agent whose `cwd` the daemon omitted:
    // absence itself, which is the one thing a daemon string cannot imitate.
    renderOverview({ snapshot: snapshotWithAgent({ cwd: undefined, tab: { kind: "dashboard" } }) });

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
    // field names but cannot close a placeholder inside an allowed field, so
    // the path the honesty claim was actually violable through is in this
    // fleet.
    snapshot.agents = [...snapshot.agents, { ...(snapshot.agents[0] as AgentSession), id: "99", displayName: "no-cwd", cwd: undefined, tab: { kind: "dashboard" } }];

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

    // The regression guard for #801 against PRD #745's suppression rule: this
    // is the one case where the status IS `connected` and the message still
    // matters, so a naive "hide the message when connected" would silently
    // undo the session-long caveat.
    expect(screen.getByTestId("daemon-state")).toHaveTextContent("Connected anyway for this session");
    // Connected means the fleet is readable again, so it is listed.
    expect(rows(document.body).length).toBeGreaterThan(0);
    expect(screen.queryByTestId("overview-connect-anyway")).not.toBeInTheDocument();
  });

  /**
   * Scenario: a healthy connection. The lamp beside the daemon's name is green
   * and the line that read `Daemon responding` next to it is gone — two
   * renderings of one bit, and the screen narrating its own state (PRD #745).
   */
  it("says nothing beside the lamp when the daemon is simply responding", () => {
    renderOverview();

    expect(screen.queryByTestId("daemon-state")).not.toBeInTheDocument();
    expect(document.body.textContent ?? "").not.toContain("Daemon responding");
    // The lamp is what says it, and it still does.
    expect(screen.getByTestId("daemon-group").querySelector(".connection-lamp.connection-connected")).not.toBeNull();
  });

  /**
   * Scenario: the two states where the message is the only explanation there
   * is. It is suppressed for a healthy connection and for nothing else — the
   * element was never the problem, the restatement was (PRD #745).
   */
  it("keeps the connection message in every state that says something the lamp does not", () => {
    for (const state of ["disconnected", "error"] as const) {
      const { unmount } = render(<AgentOverview runtime={runtime({ snapshot: createFixtureSnapshot(state) })} onNavigate={vi.fn()} />);
      expect(screen.getByTestId("daemon-state")).toBeVisible();
      unmount();
    }
  });

  /**
   * Issue #801. Two builds from different commits that name the same release
   * are compatible by this project's own bump policy, so the overview shows the
   * fleet and says nothing: no incompatible note, no Connect anyway, no second
   * banner recreating the noise this removed. The stamps are still reachable —
   * on the state line's `title`, which is a hover and not an alert.
   */
  it("shows the fleet with no alert when the differing stamps name the same release", () => {
    const snapshot = createFixtureSnapshot("crowded");
    snapshot.connection = {
      ...snapshot.connection,
      status: "connected",
      message: "Daemon responding",
      daemonDetected: true,
      runningAgentCount: 9,
      buildStampMismatchOnly: false,
      clientBuildVersion: "0.39.0-49-ga0165f8",
      daemonBuildVersion: "0.39.0-g1ea0fe7",
    };
    render(<AgentOverview runtime={runtime({ mode: "live", snapshot })} onNavigate={vi.fn()} />);

    expect(screen.queryByTestId("overview-incompatible")).not.toBeInTheDocument();
    expect(screen.queryByTestId("overview-connect-anyway")).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Incompatible daemon" })).not.toBeInTheDocument();
    expect(rows(document.body).length).toBeGreaterThan(0);

    // The connection is healthy, so its message says nothing the lamp does not
    // and no longer takes a line (PRD #745). The stamps are still reachable —
    // on the daemon's own name, which is a hover and not an alert, and which a
    // reader can actually find because it is a thing they can see.
    expect(screen.queryByTestId("daemon-state")).not.toBeInTheDocument();
    expect(screen.getByTestId("daemon-identity"))
      .toHaveAttribute("title", `${FIXTURE_DAEMON_ID} · Built from different commits — desktop 0.39.0-49-ga0165f8, daemon 0.39.0-g1ea0fe7.`);
  });

  /** Matching stamps have nothing to disclose, so only the socket path is on hover. */
  it("discloses nothing but the socket path when both builds report the same stamp", () => {
    const snapshot = createFixtureSnapshot("crowded");
    snapshot.connection = {
      ...snapshot.connection,
      status: "connected",
      message: "Daemon responding",
      daemonDetected: true,
      clientBuildVersion: "0.39.0-g1ea0fe7",
      daemonBuildVersion: "0.39.0-g1ea0fe7",
    };
    render(<AgentOverview runtime={runtime({ mode: "live", snapshot })} onNavigate={vi.fn()} />);

    expect(screen.getByTestId("daemon-identity")).toHaveAttribute("title", FIXTURE_DAEMON_ID);
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
