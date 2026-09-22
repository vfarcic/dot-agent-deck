import { describe, expect, it } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_PENDING_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import type { ConnectionView, DeckFleet } from "../types";
import {
  DECK_STATE_FALLBACK,
  deckChoices,
  deckUnavailableReason,
  directoryLabel,
  filterDirectoryEntries,
  fleetLists,
  isDeckGoneError,
  isNewAgentShortcut,
  NEW_AGENT_APPEAR_TIMEOUT_MS,
  preselectedDeck,
  seedCommand,
} from "./newAgent";

describe("New agent rules (PRD #1223 M4)", () => {
  /**
   * Scenario: a deck in each state the overview distinguishes is asked whether
   * it can take a spawn. Only a connected deck can — including one connected
   * through Connect anyway — and every other one gives its own message, or the
   * overview's sentence for its state when it carries none.
   */
  it("makes only a connected deck eligible, and gives every other one the overview's reason", () => {
    const connection = (patch: Partial<ConnectionView>): ConnectionView => ({ status: "connected", deckId: "deck-a", ...patch });
    expect(deckUnavailableReason(connection({}))).toBeUndefined();
    expect(deckUnavailableReason(connection({ buildStampMismatchOnly: true, message: "Built from different commits." }))).toBeUndefined();
    expect(deckUnavailableReason(connection({ status: "disconnected" }))).toBe(DECK_STATE_FALLBACK.disconnected);
    expect(deckUnavailableReason(connection({ status: "disconnected", message: "ssh: connect to host build-box port 22: refused" }))).toBe("ssh: connect to host build-box port 22: refused");
    expect(deckUnavailableReason(connection({ status: "error" }))).toBe(DECK_STATE_FALLBACK.incompatible);
    expect(deckUnavailableReason(connection({ status: "error", buildStampMismatchOnly: true, message: "The deck was built from another commit." }))).toBe("The deck was built from another commit.");
    expect(deckUnavailableReason(connection({ status: "loading", pending: true }))).toBe(DECK_STATE_FALLBACK.pending);
    expect(deckUnavailableReason(connection({ status: "disconnected", unconfigured: true }))).toBe(DECK_STATE_FALLBACK.unconfigured);
    expect(deckUnavailableReason(connection({ status: "loading" }))).toBe(DECK_STATE_FALLBACK.loading);
  });

  /**
   * Scenario: the four-deck fleet preview becomes deck-step rows. Every deck
   * with an identity is listed in fleet order; the unreachable and pending
   * decks carry reasons; a placeholder entry with no `deckId` is not a deck
   * and is left out.
   */
  it("lists every identified deck in fleet order with the ineligible ones' reasons", () => {
    const fleet: DeckFleet = [...createFixtureFleet("fleet"), { ...createFixtureSnapshot("connected"), connection: { status: "loading" } }];
    const choices = deckChoices(fleet);
    expect(choices.map((choice) => choice.deckId)).toEqual([FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID, FIXTURE_PENDING_DAEMON_ID]);
    expect(choices.map((choice) => choice.reason === undefined)).toEqual([true, true, false, false]);
    expect(choices[1]).toMatchObject({ deckKind: "remote" });
    expect(choices[2].reason).toBe("No deck is listening on the configured socket.");
  });

  /**
   * Scenario: the deck step opens on the deck named by a header's affordance
   * when that deck can take a spawn; otherwise on the only eligible deck when
   * there is exactly one; otherwise on nothing, so the user chooses.
   */
  it("preselects the requested deck, else the only eligible one, else none", () => {
    const fleet = deckChoices(createFixtureFleet("fleet"));
    expect(preselectedDeck(fleet, FIXTURE_REMOTE_DAEMON_ID)).toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(preselectedDeck(fleet)).toBeUndefined();
    expect(preselectedDeck(fleet, FIXTURE_UNREACHABLE_DAEMON_ID)).toBeUndefined();
    const single = deckChoices(createFixtureFleet("connected"));
    expect(preselectedDeck(single)).toBe(FIXTURE_DAEMON_ID);
    expect(preselectedDeck(single, "deck-unknown")).toBe(FIXTURE_DAEMON_ID);
    expect(preselectedDeck(deckChoices(createFixtureFleet("disconnected")))).toBeUndefined();
  });

  /**
   * Scenario: the Name prefill is the last component of the deck's canonical
   * path — for a Unix path, one with a trailing separator, a Windows path, and
   * the root, which has none.
   */
  it("labels a directory by the last component of the deck's path", () => {
    expect(directoryLabel("/home/dev/scratch")).toBe("scratch");
    expect(directoryLabel("/home/dev/scratch/")).toBe("scratch");
    expect(directoryLabel("\\\\?\\C:\\Users\\dev\\repo")).toBe("repo");
    expect(directoryLabel("/")).toBe("");
  });

  /**
   * Scenario: Command is prefilled in the TUI's order — the deck's configured
   * default command, then the last command started on that deck, then blank.
   * A whitespace-only last command counts as none.
   */
  it("prefills Command from the default command, then the last command, then blank", () => {
    expect(seedCommand("opencode", "claude")).toBe("opencode");
    expect(seedCommand(undefined, "claude --model haiku")).toBe("claude --model haiku");
    expect(seedCommand("", "claude")).toBe("claude");
    expect(seedCommand(undefined, "   ")).toBe("");
    expect(seedCommand()).toBe("");
  });

  /** Scenario: the crate's resolve refusal reads as "the deck left"; a connection error or a daemon refusal does not. */
  it("recognises the resolve refusal and nothing else as a departed deck", () => {
    expect(isDeckGoneError("that deck is not one this app is observing: deck-1234")).toBe(true);
    expect(isDeckGoneError("I/O error talking to daemon: Connection refused")).toBe(false);
    expect(isDeckGoneError("daemon returned error: unresolved: that path did not resolve")).toBe(false);
  });

  /**
   * Scenario: the fleet wait asks for the composite identity. An agent with
   * the same id on another deck does not count, and neither does the right
   * deck before it lists the agent.
   */
  it("finds a started agent only on the deck it was started on", () => {
    const withAgents = (deckId: string, ids: string[]) => ({
      ...createFixtureSnapshot("connected"),
      connection: { status: "connected" as const, deckId },
      agents: ids.map((id) => createFixtureStartedAgent({ id, daemonId: deckId })),
    });
    const fleet: DeckFleet = [withAgents(FIXTURE_DAEMON_ID, ["7"]), withAgents(FIXTURE_REMOTE_DAEMON_ID, ["1"])];
    expect(fleetLists(fleet, FIXTURE_DAEMON_ID, "7")).toBe(true);
    expect(fleetLists(fleet, FIXTURE_REMOTE_DAEMON_ID, "7")).toBe(false);
    expect(fleetLists(fleet, FIXTURE_DAEMON_ID, "1")).toBe(false);
    expect(fleetLists(fleet, "deck-unknown", "7")).toBe(false);
  });

  /** Scenario: the filter keeps names containing the query, whatever its case, and an empty filter keeps all. */
  it("filters directory names case-insensitively", () => {
    const entries = [
      { path: "/r/Alpha", displayName: "Alpha", isProject: false },
      { path: "/r/beta", displayName: "beta", isProject: true },
    ];
    expect(filterDirectoryEntries(entries, "AL").map((entry) => entry.displayName)).toEqual(["Alpha"]);
    expect(filterDirectoryEntries(entries, "")).toHaveLength(2);
    expect(filterDirectoryEntries(entries, "zzz")).toEqual([]);
  });

  /** Scenario: Ctrl+N and Cmd+N open the flow; Ctrl+Shift+N, Alt+Cmd+N and a bare N do not. */
  it("binds Ctrl+N and Cmd+N and leaves the other chords alone", () => {
    expect(isNewAgentShortcut({ key: "n", ctrlKey: true })).toBe(true);
    expect(isNewAgentShortcut({ key: "n", metaKey: true })).toBe(true);
    expect(isNewAgentShortcut({ key: "N", ctrlKey: true, shiftKey: true })).toBe(false);
    expect(isNewAgentShortcut({ key: "n", metaKey: true, altKey: true })).toBe(false);
    expect(isNewAgentShortcut({ key: "n" })).toBe(false);
    expect(isNewAgentShortcut({ key: "k", ctrlKey: true })).toBe(false);
  });

  /**
   * Scenario: the wait for a started agent is bounded comfortably above the
   * crate's five-second reconcile, which is how long a watched deck can take to
   * list a hookless agent.
   */
  it("waits comfortably longer than the five-second reconcile", () => {
    expect(NEW_AGENT_APPEAR_TIMEOUT_MS).toBeGreaterThanOrEqual(2 * 5_000);
  });
});
