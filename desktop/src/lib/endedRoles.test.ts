import { describe, expect, it } from "vitest";
import words from "../../../tests/fixtures/ended-role.json";
import { ENDED_ROLE_WORDS, endedRolesOf } from "./endedRoles";
import type { EndedRole } from "../types";

function role(roleIndex: number, agentId: string, orchestrationId = "run"): EndedRole {
  return { paneId: agentId, agentId, roleIndex, roleName: `role-${roleIndex}`, orchestrationId, orchestrationName: "orch", reason: "its pane ended", headline: "", lastOutput: [] };
}

describe("ended role words (issue #1965)", () => {
  /// Scenario: The desktop's words for an exited role are the ones the TUI prints, both read from the shared fixture.
  it("match the words the TUI prints", () => {
    expect(ENDED_ROLE_WORDS.status).toBe(words.status);
    expect(ENDED_ROLE_WORDS.lastOutput).toBe(words.lastOutput);
  });

  /// Scenario: Exited roles are kept per orchestration, skip roles a live agent holds, keep the newest generation of a role, and come out in role order.
  it("selects one orchestration's exited roles that no live agent covers", () => {
    const ended = [role(2, "5"), role(0, "1", "other"), role(1, "2"), role(1, "7"), role(3, "8")];
    expect(endedRolesOf(ended, "run", new Set([3])).map((r) => r.agentId)).toEqual(["7", "5"]);
    expect(endedRolesOf(ended, undefined, new Set())).toEqual([]);
    expect(endedRolesOf(undefined, "run", new Set())).toEqual([]);
  });
});
