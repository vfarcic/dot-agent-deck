import { describe, expect, it } from "vitest";
import type { DesktopAgentDto } from "./bridge";
import { statusFromDaemon } from "./bridge";
import { statusLabel } from "../types";
import {
  buildDashboardFilterHeader,
  clearDashboardFilter,
  DASHBOARD_STATUSES,
  dashboardFilterFromParams,
  filterDashboardAgents,
  removeDashboardFilterFacet,
  type DashboardFilter,
} from "./dashboardFilter";

// These are existing daemon facts, including its distinct status values.
// No fixture-only model, token, worktree or duration fields are involved.
type FilterAgent = Pick<DesktopAgentDto, "id" | "displayName" | "tab" | "cwd" | "lastUserPrompt" | "status" | "agentType" | "authoringKind"> & { daemonId: string };

function agent(id: string, overrides: Partial<FilterAgent> = {}): FilterAgent {
  return {
    id, daemonId: "local", displayName: id, tab: { kind: "dashboard" },
    status: "idle", agentType: "claude_code", ...overrides,
  };
}

const agents: FilterAgent[] = [
  agent("dispatcher", { tab: { kind: "mode", name: "dispatcher" }, status: "working", agentType: "codex" }),
  agent("schedule", { tab: { kind: "mode", name: "schedule" }, status: "thinking", agentType: "open_code" }),
  agent("issues", { tab: { kind: "mode", name: "schedule: issues" }, status: "waiting_for_input", agentType: "pi" }),
  agent("tester", { daemonId: "build-box", tab: { kind: "orchestration", name: "Release pipeline", roleName: "quality-checker", roleIndex: 1, isStartRole: false, orchestrationId: "run-a" }, status: "blocked", cwd: "/repos/dashboard-project", lastUserPrompt: "Fix the scroll sentinel" }),
  agent("single", { daemonId: "build-box", status: "error", agentType: "codex" }),
  agent("idle"),
];

function filter(overrides: Partial<DashboardFilter> = {}): DashboardFilter {
  return { kinds: [], statuses: [], agentTypes: [], daemonIds: [], text: "", ...overrides };
}

function ids(active: DashboardFilter, input = agents) {
  return filterDashboardAgents(input, active).map((entry: FilterAgent) => entry.id);
}

describe("dashboard filter", () => {
  /// Scenario: select each kind against a fleet containing mode tabs, an orchestration role and plain agents. A schedule filter must not accidentally include the separate schedule: issues mode.
  it.each([
    ["dispatcher", ["dispatcher"]], ["schedule", ["schedule"]],
    ["schedule-issues", ["issues"]], ["orchestration", ["tester"]],
    ["single", ["single", "idle"]],
  ] as const)("shows only the %s kind", (kind, expected) => {
    expect(ids(filter({ kinds: [kind] }))).toEqual(expected);
  });

  /// Scenario: select each user-facing status using the daemon's original facts. Thinking and Working, and Idle and Needs input, must remain independently selectable.
  it.each([
    ["working", "dispatcher"], ["thinking", "schedule"],
    ["waiting_for_input", "issues"], ["idle", "idle"],
    ["blocked", "tester"], ["error", "single"],
  ] as const)("shows only agents with status %s", (status, expected) => {
    expect(ids(filter({ statuses: [status] }))).toEqual([expected]);
  });

  /// Scenario: choose Claude Code, Codex, OpenCode and Pi in turn. Agents are matched by their daemon-reported type rather than inferred from a label or binary name.
  it.each([
    ["claude_code", ["tester", "idle"]], ["codex", ["dispatcher", "single"]],
    ["open_code", ["schedule"]], ["pi", ["issues"]],
  ] as const)("shows only agent type %s", (agentType, expected) => {
    expect(ids(filter({ agentTypes: [agentType] }))).toEqual(expected);
  });

  /// Scenario: select one or several daemons whose agents may have the same per-daemon id. Filtering preserves both identities when both daemons are selected.
  it("matches daemon identity and ORs selected daemons", () => {
    expect(ids(filter({ daemonIds: ["build-box"] }))).toEqual(["tester", "single"]);
    expect(ids(filter({ daemonIds: ["local", "build-box"] }))).toEqual(agents.map((entry) => entry.id));
    const sameId = [agent("1"), agent("1", { daemonId: "build-box" })];
    expect(filterDashboardAgents(sameId, filter())).toHaveLength(2);
    expect(filterDashboardAgents(sameId, filter({ daemonIds: ["build-box"] }))).toEqual([sameId[1]]);
  });

  /// Scenario: say "show the working agents on all daemons", which reaches the dashboard with `daemon` = the Daemon selector's All daemons id. That is every daemon, so no daemon facet is set and the working agents on every daemon stay.
  it("reads a voice All daemons param as no daemon facet", () => {
    const spoken = dashboardFilterFromParams([{ name: "status", value: "working" }, { name: "daemon", value: "all-daemons" }]);
    expect(spoken.daemonIds).toEqual([]);
    expect(ids(spoken)).toEqual(["dispatcher"]);
    expect(dashboardFilterFromParams([{ name: "daemon", value: "build-box" }]).daemonIds).toEqual(["build-box"]);
  });

  /// Scenario: type a case-insensitive, padded substring from each searchable fact. A role, orchestration, directory or prompt match keeps that agent even when its label does not contain the query.
  it.each([" TESTER ", "quality-checker", "RELEASE PIPELINE", "dashboard-project", "scroll sentinel"])(
    "searches the existing facts for %s", (text) => {
      expect(ids(filter({ text }))).toEqual(["tester"]);
    },
  );

  /// Scenario: select multiple values within every multi-select facet, then combine them. Values within one facet are alternatives while every active facet must match.
  it("ORs values within facets and ANDs the facets", () => {
    expect(ids(filter({ kinds: ["dispatcher", "single"], statuses: ["working", "error"], agentTypes: ["codex", "pi"], daemonIds: ["local", "build-box"] }))).toEqual(["dispatcher", "single"]);
    expect(ids(filter({ kinds: ["dispatcher", "single"], statuses: ["working", "error"], agentTypes: ["codex"], daemonIds: ["build-box"], text: "single" }))).toEqual(["single"]);
    expect(ids(filter({ kinds: ["dispatcher"], daemonIds: ["build-box"] }))).toEqual([]);
    expect(ids(filter({ text: " \t " }))).toEqual(agents.map((entry) => entry.id));
  });

  /// Scenario: clear a filter with all five facets active. Every agent returns in its original order and the input facts remain unchanged.
  it("clears every facet without mutating the fleet", () => {
    const original = structuredClone(agents);
    const active = filter({ kinds: ["dispatcher"], statuses: ["working"], agentTypes: ["codex"], daemonIds: ["local"], text: "dispatcher" });
    expect(ids(active)).toEqual(["dispatcher"]);
    expect(clearDashboardFilter()).toEqual(filter());
    expect(ids(clearDashboardFilter())).toEqual(agents.map((entry) => entry.id));
    expect(agents).toEqual(original);
  });

  /// Scenario: show the header while Working and Dispatchers are selected. Its separate filter summary identifies the visible count against the whole fleet and labels both active facets.
  it("states the active filter and visible count", () => {
    const header = buildDashboardFilterHeader(filter({ statuses: ["working"], kinds: ["dispatcher"] }), 4, 11);
    expect(header.summary).toBe("Showing 4 of 11 agents · Working · Dispatchers");
    expect(header.showAll).toBe(true);
    expect(header.chips.map((chip: { facet: keyof DashboardFilter }) => chip.facet)).toEqual(expect.arrayContaining(["statuses", "kinds"]));
  });

  /// Scenario: activate any one facet, including a filter matching no agents. Show all stays available until the final facet is cleared.
  it.each([
    filter({ kinds: ["dispatcher"] }), filter({ statuses: ["thinking"] }),
    filter({ agentTypes: ["codex"] }), filter({ daemonIds: ["build-box"] }), filter({ text: "absent" }),
  ])("keeps Show all available for any active facet: %j", (active) => {
    expect(buildDashboardFilterHeader(active, 0, 6).showAll).toBe(true);
    expect(buildDashboardFilterHeader(clearDashboardFilter(), 6, 6).showAll).toBe(false);
  });

  /// Scenario: remove each facet chip from a filter with all facets active. The selected facet alone is cleared and the other selections keep restricting the dashboard.
  it.each(["kinds", "statuses", "agentTypes", "daemonIds", "text"] as const)("removes only the %s chip", (facet) => {
    const active = filter({ kinds: ["dispatcher"], statuses: ["working"], agentTypes: ["codex"], daemonIds: ["local"], text: "dispatch" });
    const before = structuredClone(active);
    expect(removeDashboardFilterFacet(active, facet)).toEqual({ ...active, [facet]: facet === "text" ? "" : [] });
    expect(active).toEqual(before);
  });
});

describe("dashboard filter kinds follow what the daemon recorded", () => {
  // What a current client's authoring agents look like on the wire: a plain
  // dashboard pane, told apart only by the kind the daemon recorded when it
  // accepted the start. The legacy `mode` tab only comes from an older TUI.
  const fleet: FilterAgent[] = [
    agent("dispatcher", { authoringKind: "dispatcher" }),
    agent("schedule", { authoringKind: "schedule" }),
    agent("issues", { authoringKind: "schedule-issues" }),
    agent("legacy-dispatcher", { tab: { kind: "mode", name: "dispatcher" } }),
    agent("role", { tab: { kind: "orchestration", name: "Release", roleName: "coder", roleIndex: 0, isStartRole: true, orchestrationId: "run-b" } }),
    agent("plain"),
  ];

  /// Scenario: filter a fleet of dashboard panes the daemon recorded as a dispatcher, a schedule and a schedule-issues agent, beside a legacy dispatcher mode pane, an orchestration role and a plain agent. Each authoring kind shows its own agents, Single agents shows only the plain one, and Orchestration roles only the role.
  it.each([
    ["dispatcher", ["dispatcher", "legacy-dispatcher"]],
    ["schedule", ["schedule"]],
    ["schedule-issues", ["issues"]],
    ["single", ["plain"]],
    ["orchestration", ["role"]],
  ] as const)("shows only the %s kind", (kind, expected) => {
    expect(ids(filter({ kinds: [kind] }), fleet)).toEqual(expected);
  });

  /// Scenario: say "show only the dispatchers", which reaches the dashboard as a `filter_dashboard` dispatch with `kind` = `dispatcher`. The spoken filter shows the agent the daemon recorded as a dispatcher.
  it("agrees for a voice kind param", () => {
    const spoken = dashboardFilterFromParams([{ name: "kind", value: "dispatcher" }]);
    expect(ids(spoken, fleet)).toEqual(["dispatcher", "legacy-dispatcher"]);
  });
});

describe("dashboard filter statuses follow the status columns", () => {
  // A hookless agent reports `running`, and a compacting one `compacting`: the
  // dashboard's status column shows both as Working, so the Working filter has
  // to keep them. A word that is not one of the filter's own falls into the
  // filter status of the column it is shown in.
  const fleet: FilterAgent[] = [
    agent("hookless", { status: "running" }),
    agent("compacting", { status: "compacting" }),
    agent("shouting", { status: "WORKING" as FilterAgent["status"] }),
    agent("thinking", { status: "thinking" }),
    agent("unknown", { status: "unknown" }),
    agent("idle"),
  ];

  /// Scenario: filter a fleet holding a hookless `running` agent, a `compacting` one and an upper-case `WORKING` one by Working. All three are in the dashboard's Working column, so all three stay, and Thinking stays its own status.
  it("keeps every agent the Working column shows under the Working filter", () => {
    expect(ids(filter({ statuses: ["working"] }), fleet)).toEqual(["hookless", "compacting", "shouting"]);
    expect(ids(filter({ statuses: ["thinking"] }), fleet)).toEqual(["thinking"]);
    expect(ids(filter({ statuses: ["idle"] }), fleet)).toEqual(["unknown", "idle"]);
  });

  /// Scenario: say "show working agents", which reaches the dashboard as a `filter_dashboard` dispatch with `status` = `working`. The voice filter shows the same agents the Working column does.
  it("agrees with the columns for a voice status param", () => {
    const spoken = dashboardFilterFromParams([{ name: "status", value: "working" }]);
    expect(ids(spoken, fleet)).toEqual(["hookless", "compacting", "shouting"]);
  });
});

describe("Waiting is its own status, never Idle or Needs input (issue #1757)", () => {
  // `waiting` is the daemon's word for an agent whose turn ended while a
  // command it started in that turn still runs. It is a status the filter
  // offers by name, so neither Idle nor Needs input may take it.
  const fleet: FilterAgent[] = [
    agent("waiting", { status: "waiting" as FilterAgent["status"] }),
    agent("needs-input", { status: "waiting_for_input" }),
    agent("idle"),
    agent("working", { status: "working" }),
  ];

  /// Scenario: read the daemon's `waiting` word the way the dashboard does. It gets a column of its own, labelled `waiting` like the TUI card's Waiting, which is neither Idle nor Needs input.
  it("maps the daemon's word to its own column and label", () => {
    expect(statusFromDaemon("waiting")).toBe("waiting");
    expect(statusFromDaemon("WAITING")).toBe("waiting");
    expect(statusLabel(statusFromDaemon("waiting"))).toBe("waiting");
    expect(statusFromDaemon("waiting_for_input")).toBe("needs_input");
  });

  /// Scenario: open the status filter. It offers Waiting by that name, apart from Needs input and Idle.
  it("offers Waiting in the status filter", () => {
    const labels = DASHBOARD_STATUSES.map((status) => status.label);
    expect(DASHBOARD_STATUSES).toContainEqual({ id: "waiting", label: "Waiting" });
    expect(labels).toContain("Needs input");
    expect(labels).toContain("Idle");
    expect(new Set(labels).size).toBe(labels.length);
  });

  /// Scenario: filter a fleet holding a Waiting, a Needs input, an Idle and a Working agent by each of the first three in turn. Each filter shows exactly its own agent.
  it.each([
    ["waiting", ["waiting"]],
    ["waiting_for_input", ["needs-input"]],
    ["idle", ["idle"]],
  ] as const)("shows only the %s agent", (status, expected) => {
    expect(ids(filter({ statuses: [status as DashboardFilter["statuses"][number]] }), fleet)).toEqual(expected);
  });

  /// Scenario: say "show only the agents that are waiting", which reaches the dashboard as `status` = `waiting`. The voice filter shows the Waiting agent alone, and the header names it.
  it("agrees for a voice status param and names it in the header", () => {
    const spoken = dashboardFilterFromParams([{ name: "status", value: "waiting" }]);
    expect(ids(spoken, fleet)).toEqual(["waiting"]);
    expect(buildDashboardFilterHeader(spoken, 1, 4).summary).toBe("Showing 1 of 4 agents · Waiting");
  });
});
