import { describe, expect, it } from "vitest";
import type { DesktopAgentDto } from "./bridge";
import {
  buildDashboardFilterHeader,
  clearDashboardFilter,
  filterDashboardAgents,
  removeDashboardFilterFacet,
  type DashboardFilter,
} from "./dashboardFilter";

// These are existing daemon facts, including its distinct status values.
// No fixture-only model, token, worktree or duration fields are involved.
type FilterAgent = Pick<DesktopAgentDto, "id" | "displayName" | "tab" | "cwd" | "lastUserPrompt" | "status" | "agentType"> & { daemonId: string };

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

  /// Scenario: select each user-facing status using the daemon's original facts. Thinking and Working, and Idle and Waiting for input, must remain independently selectable.
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
