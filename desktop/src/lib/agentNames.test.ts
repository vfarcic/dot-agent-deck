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

  /** Scenario: under a run title long enough to fill the bound, the coder and the reviewer keep their own names and only the title is cut, so the two still sound different; a name longer than the bound alone is cut to it. */
  it("cuts a long run title, never the agent's name", () => {
    const title = "x".repeat(70);
    const reviewer = { ...coder("2", "run-a", title), displayName: "Reviewer" };
    const coderName = agentSpokenName(coder("1", "run-a", title), 64);
    const reviewerName = agentSpokenName(reviewer, 64);
    expect(coderName).toBe(`${"x".repeat(58)} Coder`);
    expect(reviewerName).toBe(`${"x".repeat(55)} Reviewer`);
    expect(Array.from(agentSpokenName({ ...coder("3", "run-a", "é".repeat(80)), displayName: "Reviewer" }, 64))).toHaveLength(64);
    expect(agentSpokenName(coder("1", "run-a", "prd-1401"), 64)).toBe("prd-1401 Coder");
    expect(agentSpokenName({ ...reviewer, displayName: "r".repeat(70) }, 64)).toBe("r".repeat(64));
    expect(agentSpokenName({ ...reviewer, displayName: "r".repeat(63) }, 64)).toBe("r".repeat(63));
    expect(agentSpokenName({ ...seed, displayName: "s".repeat(70), tab: { kind: "dashboard" } }, 64)).toBe("s".repeat(64));
  });
});
