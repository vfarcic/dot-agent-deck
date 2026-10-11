import { describe, expect, it } from "vitest";
import { createFixtureFleet, FIXTURE_PROMPT_KEYS, fixtureAgentIdentity } from "../data/fixture";
import { agentTurn, agentTypeLabel, voicePaneAgent } from "./promptKeys";

describe("PRD #1541 — the voice surface's view of the open agent", () => {
  it("names every agent type a refusal can mention, and none for `none`", () => {
    expect(agentTypeLabel("claude_code")).toBe("Claude Code");
    expect(agentTypeLabel("open_code")).toBe("OpenCode");
    expect(agentTypeLabel("codex")).toBe("Codex");
    expect(agentTypeLabel("pi")).toBe("Pi");
    expect(agentTypeLabel("devin")).toBe("Devin");
    expect(agentTypeLabel("none")).toBeUndefined();
    expect(agentTypeLabel(undefined)).toBeUndefined();
  });

  it("reads a turn only from statuses that say so", () => {
    for (const status of ["thinking", "working", "compacting"]) expect(agentTurn(status)).toBe("working");
    for (const status of ["idle", "waiting_for_input"]) expect(agentTurn(status)).toBe("idle");
    // `running` is a hookless agent; the rest say nothing about a turn.
    for (const status of ["running", "error", "blocked", "unknown", "something_new"]) expect(agentTurn(status)).toBeUndefined();
  });

  it("builds the pane's agent half, leaving what the deck did not say absent", () => {
    const keys = FIXTURE_PROMPT_KEYS.pi!;
    expect(voicePaneAgent({ agentType: "pi", turn: "working", promptKeys: keys })).toEqual({
      agentType: "pi",
      agentLabel: "Pi",
      turn: "working",
      promptKeys: keys,
    });
    expect(voicePaneAgent({ agentType: "devin", turn: "idle" })).toEqual({ agentType: "devin", agentLabel: "Devin", turn: "idle" });
    const unknown = voicePaneAgent({ agentType: "none" });
    expect(unknown).toEqual({ agentType: "none" });
    expect("promptKeys" in unknown).toBe(false);
    expect(voicePaneAgent({})).toEqual({});
  });

  it("gives fixture agents the keys a real deck serves, and none to Devin or an unknown binary", () => {
    expect(fixtureAgentIdentity("claude", "running")).toEqual({ agentType: "claude_code", turn: "working", promptKeys: FIXTURE_PROMPT_KEYS.claude_code });
    expect(fixtureAgentIdentity("opencode", "needs_input")).toEqual({ agentType: "open_code", turn: "idle", promptKeys: FIXTURE_PROMPT_KEYS.open_code });
    expect(fixtureAgentIdentity("devin", "running")).toEqual({ agentType: "devin", turn: "working" });
    expect(fixtureAgentIdentity("bash", "failed")).toEqual({ agentType: "none" });
    expect(fixtureAgentIdentity(undefined, "idle")).toEqual({ agentType: "none", turn: "idle" });
  });

  it("puts the identity on every agent of the fixture's scenarios", () => {
    for (const state of ["connected", "crowded", "docs", "fleet"] as const) {
      for (const deck of createFixtureFleet(state)) {
        for (const agent of deck.agents) {
          expect(agent.agentType, `${state}/${agent.id}`).toBeDefined();
          if (agent.cli === "claude") expect(agent.promptKeys).toEqual(FIXTURE_PROMPT_KEYS.claude_code);
        }
      }
    }
  });

  it("never carries Ctrl+C in the fixture's keys", () => {
    for (const keys of Object.values(FIXTURE_PROMPT_KEYS)) {
      if (!keys) continue;
      const all = [...keys.interrupt.map((step) => step.bytes), keys.clear.bytes, keys.deleteChar.bytes];
      for (const bytes of all) expect(bytes).not.toContain("\u0003");
    }
  });
});
