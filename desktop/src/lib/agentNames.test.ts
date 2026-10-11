import { describe, expect, it } from "vitest";
import { agentSpokenName, orchestrationTitle } from "./agentNames";
import { groupAgents, toOverviewAgent } from "../components/AgentOverview";
import { createFixtureSnapshot } from "../data/fixture";
import type { AgentSession } from "../types";

const seed = createFixtureSnapshot("connected").agents[0];

function coder(id: string, orchestrationId: string, displayTitle: string | undefined): AgentSession {
  return { ...seed, id, displayName: "Coder", tab: { kind: "orchestration", orchestrationId, name: "mixed", displayTitle, roleName: "coder", roleIndex: 1, isStartRole: false } };
}

/** Issue #1667 — one naming rule for the overview's orchestration heading and for what is heard about an agent. */
describe("agent names", () => {
  /** Scenario: a titled run is named by its title and an untitled one by its orchestration's name, and an agent in either is named by that plus its own name; a standalone agent keeps its own name. */
  it("names an orchestration agent by the run title and its name", () => {
    expect(agentSpokenName(coder("1", "run-a", "prd-1401"))).toBe("prd-1401 Coder");
    expect(agentSpokenName(coder("2", "run-b", undefined))).toBe("mixed Coder");
    expect(agentSpokenName(coder("3", "run-c", ""))).toBe("mixed Coder");
    expect(agentSpokenName({ ...seed, displayName: "Reviewer", tab: { kind: "dashboard" } })).toBe("Reviewer");
    expect(agentSpokenName({ ...seed, displayName: "Reviewer", tab: { kind: "mode", name: "review" } })).toBe("Reviewer");
  });

  /** Scenario: the overview's group heading for each run is the title the spoken name starts with. */
  it("starts the spoken name with the overview's group heading", () => {
    const agents = [coder("1", "run-a", "prd-1401"), coder("2", "run-b", undefined)];
    const headings = groupAgents(agents.map(toOverviewAgent)).map((group) => group.title);
    expect(headings).toEqual(["prd-1401", "mixed"]);
    for (const [index, agent] of agents.entries()) {
      if (agent.tab.kind !== "orchestration") throw new Error("not an orchestration agent");
      expect(orchestrationTitle(agent.tab)).toBe(headings[index]);
      expect(agentSpokenName(agent)).toBe(`${headings[index]} ${agent.displayName}`);
    }
  });
});
