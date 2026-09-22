import { describe, expect, it } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_PENDING_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import type { AgentSession, ConnectionView, DeckFleet, NewAgentOptions } from "../types";
import {
  ambiguousOrchestrationReason,
  AUTHORING_WITHHELD,
  authoringModes,
  cleanupWarning,
  CLEANUP_WARNING_MAX_NAMES,
  DECK_STATE_FALLBACK,
  deckChoices,
  deckUnavailableReason,
  directoryLabel,
  filterDirectoryEntries,
  fleetLists,
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
  UNNAMED_CLEANUP_ROLE,
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
   * Scenario (PRD #1223 U1): a connected deck that does not advertise
   * `list-directories` carries the crate's `newAgentReason`. Browsing is the
   * only way the flow chooses a directory, so that deck is ineligible with
   * the crate's sentence — and a deck without the reason stays eligible.
   */
  it("makes a connected deck without the listing verb ineligible with the crate's reason", () => {
    const reason = "This deck does not advertise list-directories, so it cannot be browsed for a directory to start in. Start agents on it from the TUI on its host, or upgrade the deck.";
    expect(deckUnavailableReason({ status: "connected", deckId: "deck-a", newAgentReason: reason })).toBe(reason);
    const fleet: DeckFleet = createFixtureFleet("fleet").map((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID ? { ...deck, connection: { ...deck.connection, newAgentReason: reason } } : deck);
    const choices = deckChoices(fleet);
    expect(choices.find((choice) => choice.deckId === FIXTURE_REMOTE_DAEMON_ID)?.reason).toBe(reason);
    expect(choices.find((choice) => choice.deckId === FIXTURE_DAEMON_ID)?.reason).toBeUndefined();
    expect(preselectedDeck(choices, FIXTURE_REMOTE_DAEMON_ID)).toBe(FIXTURE_DAEMON_ID);
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
   * Scenario (PRD #1223 audit V3): a project names a role after the resolve
   * refusal. The role name is interpolated into every sentence its launch
   * fails with, so a substring test would read those failures as deck loss —
   * the refusal is the whole message where it is real, so only a leading match
   * counts.
   */
  it("does not read a refusal that merely quotes the resolve wording as a departed deck", () => {
    const hostile = "that deck is not one this app is observing";
    expect(isDeckGoneError(`failed to start orchestration role ${hostile}: refused; stopped 1 already-started role(s)`)).toBe(false);
    expect(isDeckGoneError(`roles already started: ${hostile}: deck-1234`)).toBe(false);
    expect(isDeckGoneError(`${hostile}: deck-1234`)).toBe(true);
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

  /**
   * Scenario (PRD #1223 audit F2): a project defines `loop` twice and `solo`
   * once. The launch identifies an orchestration by name and the deck takes
   * the first definition, so only `solo` is offered; both `loop`s come back as
   * ambiguous, in the project's order, with a reason that says what to do.
   * Names compare exactly — `Loop` is not a namesake of `loop`.
   */
  it("offers only uniquely named orchestrations and returns every namesake as ambiguous", () => {
    const orchestration = (name: string, roles: string[]) => ({ name, displayName: name, default: false, roles: roles.map((role, index) => ({ name: role, displayName: role, start: index === 0 })) });
    const first = orchestration("loop", ["planner"]);
    const solo = orchestration("solo", ["worker"]);
    const second = orchestration("loop", ["reviewer"]);
    const cased = orchestration("Loop", ["other"]);

    expect(orchestrationModes({ kind: "project", path: "/p", displayPath: "/p", displayName: "p", orchestrations: [first, solo, second, cased] })).toEqual({ offered: [solo, cased], ambiguous: [first, second] });
    expect(ambiguousOrchestrationReason("loop")).toBe("This project defines more than one orchestration named loop; rename one to launch it here.");
    expect(ambiguousOrchestrationReason("lo\u202Eop")).toContain("named loop;");
  });
});

describe("New agent rules — cleanup the launch could not confirm (PRD #1223 audit F6)", () => {
  /**
   * Scenario: a failed launch could not confirm two roles stopped, each with
   * a 128-character name. The summary leads with the count and what to do and
   * fits the message budget, and each name is carried on its own — a joined
   * list clamped as one sentence lost the later identities silently (audit V7).
   */
  it("keeps the count and the instruction ahead of long role names, each name on its own", () => {
    const long = (prefix: string) => `${prefix}${"r".repeat(128 - prefix.length)}`;
    const warning = cleanupWarning([long("reviewer-"), long("planner-")]);

    expect(warning.summary).toBe("2 roles may still be running on this deck: their stops could not be confirmed. Check the deck and stop them there.");
    expect(Array.from(warning.summary).length).toBeLessThanOrEqual(240);
    expect(warning.names).toEqual([long("reviewer-"), long("planner-")]);
    expect(warning.names.every((name) => Array.from(name).length <= 128)).toBe(true);
    expect(warning.overflow).toBe(0);
    expect(cleanupWarning(["builder"])).toEqual({
      summary: "1 role may still be running on this deck: its stop could not be confirmed. Check the deck and stop it there.",
      names: ["builder"],
      overflow: 0,
    });
    expect(cleanupWarning(["plan\u202Ener"]).names).toEqual(["planner"]);
  });

  /**
   * Scenario (PRD #1223 audit W5): a rollback could not confirm two roles, and
   * one of them is named entirely of characters that render as nothing. Its
   * list item must say something — a blank `<li>` under "2 roles may still be
   * running on this deck" leaves the reader counting bullets to find out that
   * a role was named at all, which is the moment they most need the name.
   *
   * `displayText` retains those characters on purpose (stripping them would
   * corrupt emoji sequences and Persian, Arabic and Indic orthography), so the
   * fix is `displayIdentity`'s visible fallback and not a wider filter.
   */
  it("names a role that renders as nothing rather than listing a blank line", () => {
    expect(cleanupWarning(["\u200B\u200B", "coder"]).names).toEqual([UNNAMED_CLEANUP_ROLE, "coder"]);
    // Blankness is judged before the clamp, so padding does not rescue it —
    // and one visible character is enough to keep the real name.
    expect(cleanupWarning(["\u200Bcoder"]).names).toEqual(["\u200Bcoder"]);
    expect(cleanupWarning([" "]).names).toEqual([UNNAMED_CLEANUP_ROLE]);
  });

  /**
   * Scenario (PRD #1223 audit V7): a rollback of a large orchestration could
   * not confirm more roles than one alert should list. The count is the truth
   * the reader needs, so it is in the summary and again as the honest remainder
   * — no name is dropped without being counted.
   */
  it("counts the roles past the cap instead of dropping them", () => {
    const stops = Array.from({ length: CLEANUP_WARNING_MAX_NAMES + 5 }, (_, index) => `role-${index}`);
    const warning = cleanupWarning(stops);

    expect(warning.summary).toContain(`${stops.length} roles may still be running`);
    expect(warning.names).toEqual(stops.slice(0, CLEANUP_WARNING_MAX_NAMES));
    expect(warning.overflow).toBe(5);
  });
});
