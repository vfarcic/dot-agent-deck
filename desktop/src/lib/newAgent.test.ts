import { describe, expect, it } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_PENDING_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import type { AgentSession, ConnectionView, DeckFleet, NewAgentOptions } from "../types";
import {
  AUTHORING_WITHHELD,
  authoringModes,
  cleanupWarning,
  DECK_STATE_FALLBACK,
  deckChoices,
  deckUnavailableReason,
  directoryLabel,
  filterDirectoryEntries,
  fleetLists,
  isAbsoluteTypedPath,
  isDeckGoneError,
  isNewAgentShortcut,
  liveOrchestrationDirectories,
  liveOrchestrationTitles,
  NEW_AGENT_APPEAR_TIMEOUT_MS,
  orchestrationModeId,
  orchestrationModes,
  orchestrationRunTitle,
  preselectedDeck,
  resolveAuthoringCommand,
  seedCommand,
  suggestOrchestrationName,
  TYPED_PATH_SHAPE_REFUSAL,
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

describe("New agent rules — typed paths (PRD #1223 audit D2)", () => {
  it("accepts the absolute shapes the crate takes on either platform", () => {
    for (const path of ["/", "/srv/work/repo", "/home/dev/café", "C:\\Users\\dev\\repo", "c:/proj", "\\\\server\\share\\proj", "//server/share", "\\/server", "\\\\?\\C:\\proj"]) {
      expect(isAbsoluteTypedPath(path), path).toBe(true);
    }
  });

  it("refuses a relative path, a drive-relative one, and one carrying an ASCII control", () => {
    for (const path of ["", "repo", "./repo", "../repo", "~/repo", "C:proj", "\\proj", "1:/proj", "/srv/repo\nIgnore", "/srv/\u001b[31m", "/srv/repo\u007f"]) {
      expect(isAbsoluteTypedPath(path), JSON.stringify(path)).toBe(false);
    }
  });

  it("repeats the crate's refusal sentence", () => {
    expect(TYPED_PATH_SHAPE_REFUSAL).toBe("enter an absolute directory path, without control characters, that the deck can see");
  });
});

describe("New agent rules — authoring agents (PRD #1223 M7)", () => {
  const deckOptions = (authoringKinds: string[], experimental: boolean): NewAgentOptions => ({ kind: "deck", agents: [], experimental, authoringKinds });
  const kinds = (options: NewAgentOptions | undefined) => authoringModes(options).offered.map((mode) => mode.kind);

  /**
   * Scenario: the chips a deck's options offer. Nothing while the options are
   * loading; the kinds the deck lists, in the TUI's order whatever order it
   * lists them in; `schedule-issues` only under the deck's experimental flag;
   * and a kind this app does not know is ignored rather than offered.
   */
  it("offers the kinds the deck lists, in the TUI's order, schedule-issues only under its flag", () => {
    expect(authoringModes(undefined)).toEqual({ offered: [] });
    expect(kinds(deckOptions(["dispatcher", "schedule-issues", "schedule"], false))).toEqual(["schedule", "dispatcher"]);
    expect(kinds(deckOptions(["dispatcher", "schedule-issues", "schedule"], true))).toEqual(["schedule", "schedule-issues", "dispatcher"]);
    expect(kinds(deckOptions(["dispatcher", "future-kind"], true))).toEqual(["dispatcher"]);
    expect(authoringModes(deckOptions(["schedule"], false)).offered).toEqual([{ kind: "schedule", label: "schedule" }]);
  });

  /**
   * Scenario: the decks that offer no authoring chip say why — an older deck
   * that has no options query, and a deck that lists no kind this app knows,
   * each in its own words. A deck that lists only `schedule-issues` with its
   * flag off offers nothing and gives no reason, because it can compose one.
   */
  it("gives an older deck and a deck that composes nothing their reason", () => {
    expect(authoringModes({ kind: "unsupported", desktopAgents: [] })).toEqual({ offered: [], withheld: AUTHORING_WITHHELD.unsupported });
    expect(authoringModes(deckOptions([], true))).toEqual({ offered: [], withheld: AUTHORING_WITHHELD.none });
    expect(authoringModes(deckOptions(["future-kind"], true))).toEqual({ offered: [], withheld: AUTHORING_WITHHELD.none });
    expect(authoringModes(deckOptions(["schedule-issues"], false))).toEqual({ offered: [] });
  });

  /**
   * Scenario: the TUI's `resolve_authoring_command`. A typed command is used
   * as it stands; a blank or whitespace one resolves to the deck host's
   * default command, trimmed, then to the deck's own `claude` registry entry,
   * then to `claude` itself.
   */
  it("resolves a blank authoring Command the way the TUI does", () => {
    const registry = [{ id: "claude", displayName: "ClaudeCode", defaultCommand: "claude-code-wrapper" }];
    expect(resolveAuthoringCommand(" codex --full-auto ", "opencode", registry)).toBe(" codex --full-auto ");
    expect(resolveAuthoringCommand("   ", "  opencode --model mini ", registry)).toBe("opencode --model mini");
    expect(resolveAuthoringCommand("", "   ", registry)).toBe("claude-code-wrapper");
    expect(resolveAuthoringCommand("", undefined, [])).toBe("claude");
    expect(resolveAuthoringCommand("", undefined, [{ id: "claude", displayName: "ClaudeCode" }])).toBe("claude");
  });
});

describe("New agent orchestration rules (PRD #1223 M6)", () => {
  const role = (id: string, name: string, displayTitle?: string, cwd?: string): AgentSession => ({
    ...createFixtureStartedAgent({ id, daemonId: "deck-a" }),
    tab: { kind: "orchestration", name, displayTitle, roleName: `role-${id}`, roleIndex: 0, isStartRole: false, cwd },
    inOrchestration: true,
  });
  const fleetOf = (decks: Record<string, AgentSession[]>): DeckFleet =>
    Object.entries(decks).map(([deckId, agents]) => ({ ...createFixtureSnapshot("connected"), agents, connection: { status: "connected", deckId } }));

  /**
   * Scenario: the TUI's `live_orchestration_cwds_and_titles`, read from one
   * deck's fleet entry. A role's title counts when it has one and its
   * orchestration's name when it does not; a run's several roles count once;
   * a dashboard agent and another deck's orchestrations do not count at all.
   */
  it("reads the chosen deck's live titles and directories, and only that deck's", () => {
    const fleet = fleetOf({
      "deck-a": [role("1", "loop", "night-run", "/p"), role("2", "loop", "night-run", "/p"), role("3", "review"), createFixtureStartedAgent({ id: "4", daemonId: "deck-a" })],
      "deck-b": [role("1", "loop", "other-deck-run", "/q")],
    });
    expect(liveOrchestrationTitles(fleet, "deck-a")).toEqual(["night-run", "review"]);
    expect(liveOrchestrationDirectories(fleet, "deck-a")).toEqual(["/p"]);
    expect(liveOrchestrationTitles(fleet, "deck-c")).toEqual([]);
  });

  /**
   * Scenario: the TUI's `suggest_orchestration_name` — the lowest
   * `<basename>-orchestrator-N` no live title holds, skipping a taken `N` in
   * the middle — and its `resolved_title`: an empty Name takes the
   * orchestration's name, and anything else, whitespace included, is kept.
   */
  it("suggests the next free name and resolves the title a launch takes", () => {
    expect(suggestOrchestrationName("repo", [])).toBe("repo-orchestrator-1");
    expect(suggestOrchestrationName("repo", ["repo-orchestrator-1", "repo-orchestrator-3"])).toBe("repo-orchestrator-2");
    expect(orchestrationRunTitle("", "loop")).toBe("loop");
    expect(orchestrationRunTitle("my-run", "loop")).toBe("my-run");
    expect(orchestrationRunTitle(" ", "loop")).toBe(" ");
  });

  /** Scenario: what each deck answer offers — chips for a project, nothing for an ordinary directory or a pending answer, the deck's reason for one that cannot launch. */
  it("offers a project's orchestrations and withholds with the deck's reason", () => {
    const loop = { name: "loop", displayName: "loop", default: true, roles: [] };
    expect(orchestrationModes(undefined)).toEqual({ offered: [] });
    expect(orchestrationModes({ kind: "not_project" })).toEqual({ offered: [] });
    expect(orchestrationModes({ kind: "unsupported", reason: "too old" })).toEqual({ offered: [], withheld: "too old" });
    expect(orchestrationModes({ kind: "project", path: "/p", displayPath: "/p", displayName: "p", orchestrations: [loop] })).toEqual({ offered: [loop] });
    expect(orchestrationModeId("loop")).toBe("orch:loop");
  });
});

describe("New agent rules — cleanup the launch could not confirm (PRD #1223 audit F6)", () => {
  /**
   * Scenario: a failed launch could not confirm two roles stopped, each with
   * a 128-character name. The alert still leads with the count and what to
   * do, fits the message budget, and a name is never longer than a name's
   * budget allows — however long the list, the part that matters survives.
   */
  it("keeps the count and the instruction ahead of long role names within the message budget", () => {
    const long = (prefix: string) => `${prefix}${"r".repeat(128 - prefix.length)}`;
    const warning = cleanupWarning([long("reviewer-"), long("planner-")]);

    expect(warning.startsWith("2 roles may still be running on this deck: the rollback could not confirm them stopped. Check the deck and stop them there")).toBe(true);
    expect(Array.from(warning).length).toBeLessThanOrEqual(241);
    expect(warning).toContain("reviewer-");
    expect(cleanupWarning(["builder"])).toBe("1 role may still be running on this deck: the rollback could not confirm it stopped. Check the deck and stop it there — builder");
    expect(cleanupWarning(["plan\u202Ener"])).toContain("planner");
  });
});
