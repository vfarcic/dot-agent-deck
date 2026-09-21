import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useHeldAgentRecord } from "./useHeldAgentRecord";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import type { AgentSession } from "../types";

/** A second deck, because an agent id alone names an agent on every deck. */
const OTHER_DECK = "deck-00000000000000b2";

/**
 * A real fixture record, cloned per call the way `mapDesktopSnapshot` builds a
 * fresh object per snapshot — so an identity comparison inside the hook sees
 * what it would see in the app rather than a value that never changes.
 */
function record(id = "planner", deckId = FIXTURE_DAEMON_ID): AgentSession {
  const agent = createFixtureSnapshot("connected").agents.find((candidate) => candidate.id === id)!;
  return { ...agent, daemonId: deckId };
}

/**
 * Issue #1143 — the record an open pane's deck last gave, kept so the pane can
 * still be a pane while that deck is not answering.
 *
 * # Why the hook is driven directly rather than through the pane
 *
 * `AgentPaneNoTerminal.test.tsx` covers what the USER meets: a pane whose deck
 * stops answering, what it renders and what it says about how old it is. What
 * that cannot reach is the two properties the hold must have to be safe rather
 * than merely useful — that a record is never read back under another pane's
 * identity, and that a live record always outranks a held one — because both
 * are about transitions the app makes in one commit.
 *
 * The identity fence is the one worth stating in full. The record carries the
 * `(deckId, agentId)` it is FOR and is compared during RENDER, not cleared in
 * an effect, because an effect runs after the commit it would be correcting:
 * the first paint of a newly-opened pane is exactly the commit that would show
 * the previous pane's agent. That is the composite-identity confusion PRD
 * #1105's security audit is about, reached by a new route.
 */
describe("useHeldAgentRecord", () => {
  const target = { deckId: FIXTURE_DAEMON_ID, agentId: "planner" };

  /**
   * Scenario: the pane's deck reports the record, then stops reporting
   * anything. The hook returns nothing while the deck is answering — a caller
   * reads `live ?? held`, so a held record present at all means the one on
   * screen is older than now — and hands the last one back once it is not.
   */
  it("hands back the last record once the deck stops reporting one", () => {
    const live = record();
    const { result, rerender } = renderHook(
      ({ agent }: { agent: AgentSession | undefined }) => useHeldAgentRecord(target, agent, () => 1_000),
      { initialProps: { agent: live as AgentSession | undefined } },
    );

    expect(result.current).toBeUndefined();

    rerender({ agent: undefined });

    expect(result.current?.agent).toBe(live);
    expect(result.current?.confirmedAt).toBe(1_000);
  });

  /**
   * Scenario: the deck answers again. The live record wins immediately, so the
   * pane stops hedging on the same commit the record arrives — the mirror of
   * the defect, and just as wrong: a pane still saying `last seen` under a
   * terminal that is streaming would be lying in the other direction.
   */
  it("surrenders the hold the moment a live record returns", () => {
    const { result, rerender } = renderHook(
      ({ agent }: { agent: AgentSession | undefined }) => useHeldAgentRecord(target, agent, () => 1_000),
      { initialProps: { agent: record() as AgentSession | undefined } },
    );

    rerender({ agent: undefined });
    expect(result.current).toBeDefined();

    rerender({ agent: record() });
    expect(result.current).toBeUndefined();
  });

  /**
   * Scenario: a pane is open on one deck's `planner` and reports its record;
   * the user closes it and opens the SAME agent id on another deck, which is
   * not answering. Nothing is handed back, because nothing has been held for
   * that deck's `planner`.
   *
   * This is the case that makes the identity fence load-bearing rather than
   * tidy: agent ids are per-daemon monotonic integers, so `planner` on
   * build-box and `planner` on this machine are two different agents on two
   * different machines wearing one name.
   */
  it("never hands a record back under another deck's pane of the same id", () => {
    const { result, rerender } = renderHook(
      ({ pane, agent }: { pane: { deckId: string; agentId: string }; agent: AgentSession | undefined }) =>
        useHeldAgentRecord(pane, agent, () => 1_000),
      { initialProps: { pane: target, agent: record() as AgentSession | undefined } },
    );

    rerender({ pane: { deckId: OTHER_DECK, agentId: "planner" }, agent: undefined });

    expect(result.current).toBeUndefined();
  });

  /** Scenario: the same fence for a different agent on the pane's own deck. */
  it("never hands a record back under another agent's pane on the same deck", () => {
    const { result, rerender } = renderHook(
      ({ pane, agent }: { pane: { deckId: string; agentId: string }; agent: AgentSession | undefined }) =>
        useHeldAgentRecord(pane, agent, () => 1_000),
      { initialProps: { pane: target, agent: record() as AgentSession | undefined } },
    );

    rerender({ pane: { deckId: FIXTURE_DAEMON_ID, agentId: "reviewer" }, agent: undefined });

    expect(result.current).toBeUndefined();
  });

  /**
   * Scenario: the pane is closed, and later a pane is opened for the same agent
   * on a deck that is not answering. Nothing survives the close.
   *
   * The hold is scoped to the pane the user opened, not to the app's lifetime:
   * a record kept across a close is one waiting to be shown for a pane nobody
   * had open when it was true.
   */
  it("drops the record when the pane closes", () => {
    const { result, rerender } = renderHook(
      ({ pane, agent }: { pane: { deckId: string; agentId: string } | undefined; agent: AgentSession | undefined }) =>
        useHeldAgentRecord(pane, agent, () => 1_000),
      { initialProps: { pane: target as { deckId: string; agentId: string } | undefined, agent: record() as AgentSession | undefined } },
    );

    rerender({ pane: undefined, agent: undefined });
    rerender({ pane: target, agent: undefined });

    expect(result.current).toBeUndefined();
  });

  /**
   * Scenario: the deck keeps answering for a while and then goes away. The
   * instant reported is when it LAST answered, not when it first did.
   *
   * The distinction is the whole value of the number: `mapDesktopSnapshot`
   * builds fresh objects per snapshot, so a hold that stamped only on the first
   * report would date the record to the moment the pane opened and read
   * arbitrarily stale for a deck that was healthy the entire time.
   */
  it("dates the record to the deck's last report, not its first", () => {
    let now = 1_000;
    const { result, rerender } = renderHook(
      ({ agent }: { agent: AgentSession | undefined }) => useHeldAgentRecord(target, agent, () => now),
      { initialProps: { agent: record() as AgentSession | undefined } },
    );

    now = 9_000;
    rerender({ agent: record() });
    now = 12_000;
    rerender({ agent: undefined });

    expect(result.current?.confirmedAt).toBe(9_000);
  });
});
