import { describe, expect, it } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import type { AgentSession, DeckSnapshot } from "../types";
import { AGENTS_STOPPED_BY_RESTART, agentsStoppedByRestartNotice } from "./agentIdentity";
import { mapDesktopSnapshot, type DesktopSnapshotDto } from "./bridge";
import { observeRestarts, type HeldListings } from "./restartDrop";

/*
 * PRD #1666 part 2 (G-fix7c) — a deck that comes back as another daemon
 * process restarted: its earlier agents are gone, and the app names them in
 * one notice, in the words the TUI uses.
 */

const DECK = "deck-000000000000dec7";

function agent(id: string, displayName: string, incarnation: string | undefined, tab: AgentSession["tab"] = { kind: "dashboard" }): AgentSession {
  const base = createFixtureSnapshot("connected").agents[0];
  return { ...base, id, displayName, daemonId: DECK, tab, ...(incarnation === undefined ? { incarnation: undefined } : { incarnation }) };
}

const lead = (incarnation: string | undefined) => agent("1", "lead", incarnation, { kind: "orchestration", name: "team", displayTitle: "PRD 9 · team", roleName: "orchestrator", roleIndex: 0, isStartRole: true });
const solo = (incarnation: string | undefined) => agent("2", "api-fix", incarnation);

function deck(status: "connected" | "disconnected", incarnation: string | undefined, agents: AgentSession[]): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return { ...snapshot, connection: { ...snapshot.connection, status, deckId: DECK, ...(incarnation === undefined ? {} : { incarnation }) }, agents };
}

/** Feed `fleets` in order and answer the stopped agents each one reported. */
function run(...decks: DeckSnapshot[]) {
  let held: HeldListings = new Map();
  return decks.map((one) => {
    const result = observeRestarts(held, [one]);
    held = result.held;
    return result.stopped;
  });
}

describe("the restart notice's words", () => {
  /** Scenario: the notice names each stopped agent by label, with its role and orchestration when it had them, in the TUI's words. */
  it("names each agent with its role and orchestration", () => {
    expect(AGENTS_STOPPED_BY_RESTART).toBe("The deck restarted; these agents were stopped");
    expect(agentsStoppedByRestartNotice([
      { label: "lead", role: "orchestrator", orchestration: "team" },
      { label: "coder", role: "worker" },
      { label: "docs", orchestration: "team" },
      { label: "api-fix" },
    ])).toBe("The deck restarted; these agents were stopped: lead (orchestrator in team), coder (worker), docs (in team), api-fix.");
  });
});

describe("observeRestarts", () => {
  /** Scenario: a connected deck listed by process A comes back listed by process B. Every agent A listed is reported, an orchestration's with its role and run title. */
  it("reports every agent the previous process listed", () => {
    const [first, second] = run(deck("connected", "a", [lead("a"), solo("a")]), deck("connected", "b", [solo("b")]));
    expect(first).toEqual([]);
    expect(second).toEqual([{ label: "lead", role: "orchestrator", orchestration: "PRD 9 · team" }, { label: "api-fix" }]);
  });

  /** Scenario: the common restart — the deck goes away, then comes back as another process listing no agents at all. The restart is still seen, by the process the connection names. */
  it("sees a restart across a disconnection to a deck that lists nobody", () => {
    const reports = run(deck("connected", "a", [lead("a"), solo("a")]), deck("disconnected", undefined, []), deck("connected", "b", []));
    expect(reports).toEqual([[], [], [{ label: "lead", role: "orchestrator", orchestration: "PRD 9 · team" }, { label: "api-fix" }]]);
  });

  /** Scenario: control — the deck's link drops and it reconnects to the SAME process. Nothing is reported, and a later snapshot of it reports nothing either. */
  it("reports nothing for a reconnect to the same process", () => {
    const reports = run(deck("connected", "a", [lead("a"), solo("a")]), deck("disconnected", undefined, []), deck("connected", "a", [lead("a")]), deck("connected", "a", []));
    expect(reports).toEqual([[], [], [], []]);
  });

  /** Scenario: a deck that names no process, before or after, cannot be told restarted, so nothing is reported; nor for a deck seen for the first time. */
  it("reports nothing when either process is unnamed", () => {
    expect(run(deck("connected", undefined, [solo(undefined)]), deck("connected", "b", []))).toEqual([[], []]);
    expect(run(deck("connected", "a", [solo("a")]), deck("connected", undefined, []))).toEqual([[], []]);
  });

  /** Scenario: one restart is reported once: the snapshots after it are the new process's own. */
  it("reports one restart once", () => {
    const reports = run(deck("connected", "a", [solo("a")]), deck("connected", "b", []), deck("connected", "b", [solo("b")]));
    expect(reports.map((stopped) => stopped.length)).toEqual([0, 1, 0]);
  });
});

describe("mapDesktopSnapshot", () => {
  const dto = (status: "connected" | "disconnected"): DesktopSnapshotDto => ({
    connection: { status, deckId: DECK, socketPath: "/tmp/deck.sock", deckKind: "local", clientProtocolVersion: 6, clientBuildVersion: "0.1.0", incarnation: "process-b" },
    agents: [],
    protocolVersion: 6,
    source: "daemon",
  });

  /** Scenario: a connected snapshot carries the process that listed it even with no agents; one that is not connected carries none. */
  it("carries the process of a connected deck's listing only", () => {
    expect(mapDesktopSnapshot(dto("connected")).connection.incarnation).toBe("process-b");
    expect(mapDesktopSnapshot(dto("disconnected")).connection).not.toHaveProperty("incarnation");
  });
});
