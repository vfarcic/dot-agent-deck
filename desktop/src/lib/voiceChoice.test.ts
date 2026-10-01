import { describe, expect, it } from "vitest";
import { answerChoiceLocally, collidingChoiceEntry } from "./voiceChoice";

/* PRD #1261 — the webview's port of `voice::choice::answer`, for a runtime
   with no Rust behind it. Same order and closed lists as the Rust function. */
describe("answerChoiceLocally", () => {
  const offered = ["docs-site", "docs-api", "billing"].map((name) => ({
    name: "dir", kind: "dir_ref", spoken: "docs", value: `/code/${name}`, label: name,
  }));

  /** Scenario: whole ordinals pick an offered entry; one past the list is refused, and an ordinal inside a sentence is not an answer. */
  it("reads whole-utterance ordinals only", () => {
    for (const [said, at] of [["two", 1], ["2", 1], ["number 3", 2], ["the second one", 1], ["okay the last one please", 2], ["3rd", 2]] as const) {
      expect(answerChoiceLocally(said, offered)).toEqual({ kind: "selected", candidate: offered[at] });
    }
    expect(answerChoiceLocally("number 4", offered)).toEqual({ kind: "refused" });
    expect(answerChoiceLocally("open the second tab", offered)).toEqual({ kind: "not_answer" });
  });

  /** Scenario: a name picks the one offered entry it matches; a name matching several is refused; anything else is not an answer. */
  it("matches names among the offered entries only", () => {
    expect(answerChoiceLocally("docs-api", offered)).toEqual({ kind: "selected", candidate: offered[1] });
    expect(answerChoiceLocally("docs", offered)).toEqual({ kind: "refused" });
    expect(answerChoiceLocally("type on", offered)).toEqual({ kind: "not_answer" });
  });

  const agents = [
    { name: "agent", kind: "agent_ref" as const, spoken: "agent", value: "planner", label: "Plan / architecture", names: ["planner"] },
    { name: "agent", kind: "agent_ref" as const, spoken: "agent", value: "builder", label: "Desktop implementation", names: ["builder"] },
  ];

  /** Scenario: the offered planner can be named by a spoken name supplied with the choice even though its label reads Plan / architecture.
   * A stop command or extra word is not a choice answer, and a name matching two offered agents is refused. */
  it("does not answer an open-agent choice with a stop command", () => {
    expect(answerChoiceLocally("Plan / architecture", agents)).toEqual({ kind: "selected", candidate: agents[0] });
    expect.soft(answerChoiceLocally("planner", agents)).toEqual({ kind: "selected", candidate: agents[0] });
    expect(answerChoiceLocally("stop planner", agents)).toEqual({ kind: "not_answer" });
    expect(answerChoiceLocally("planner extra", agents)).toEqual({ kind: "not_answer" });
    const similarlyNamed = [
      { ...agents[0], value: "planner-one", names: ["planner-one"] },
      { ...agents[1], value: "planner-two", names: ["planner-two"] },
    ];
    expect.soft(answerChoiceLocally("planner", similarlyNamed)).toEqual({ kind: "refused" });
  });

  /** Scenario: an agent ID without a supplied spoken name does not answer a choice when its visible label differs. */
  it("does not treat the candidate value as a spoken name", () => {
    const withoutNames = { name: "agent", kind: "agent_ref", spoken: "agent", value: "planner", label: "Plan / architecture" };
    expect(answerChoiceLocally("planner", [withoutNames])).toEqual({ kind: "not_answer" });
  });

  /** Scenario: saying a full offered label with an extra word does not select it, matching the Rust choice answer. */
  it("does not select a label followed by extra words", () => {
    expect(answerChoiceLocally("desktop implementation extra", agents)).toEqual({ kind: "not_answer" });
  });

  /** Scenario: the closed cancel phrases cancel when said on their own, and not inside a command. */
  it("cancels only on a whole cancel phrase", () => {
    for (const said of ["cancel", "never mind", "none of them", "no"]) expect(answerChoiceLocally(said, offered)).toEqual({ kind: "cancelled" });
    expect(answerChoiceLocally("do not cancel the build", offered)).toEqual({ kind: "not_answer" });
  });

  /** Scenario: a visible name that also means a bare ordinal or cancel is
   * ambiguous and refused; an explicit numbered choice can still select it. */
  it("refuses offered labels that collide with bare choice controls", () => {
    const names = [
      { name: "agent", kind: "agent_ref" as const, spoken: "two", value: "agent-two", label: "two" },
      { name: "agent", kind: "agent_ref" as const, spoken: "Other", value: "agent-other", label: "Other" },
    ];
    expect(answerChoiceLocally("number one", names)).toEqual({ kind: "selected", candidate: names[0] });
    const cancel = [{ name: "agent", kind: "agent_ref" as const, spoken: "cancel", value: "agent-cancel", label: "cancel" }];
    expect([answerChoiceLocally("two", names), answerChoiceLocally("cancel", cancel)]).toEqual([
      { kind: "refused" }, { kind: "refused" },
    ]);
  });

  /** Scenario: Qodo on PR #1451 — an entry labelled "Builder" also answers to
   * "two" (its role), another labelled "Reviewer" to "cancel". A bare "two" or
   * "cancel" is refused and names that entry, as a colliding label is. */
  it("refuses bare controls that collide with an entry's other spoken name", () => {
    const named = [
      { name: "agent", kind: "agent_ref" as const, spoken: "x", value: "agent-builder", label: "Builder", names: ["Builder", "two", "agent-builder"] },
      { name: "agent", kind: "agent_ref" as const, spoken: "x", value: "agent-reviewer", label: "Reviewer", names: ["Reviewer", "cancel"] },
      { name: "agent", kind: "agent_ref" as const, spoken: "x", value: "agent-other", label: "Other", names: ["Other"] },
    ];
    expect([answerChoiceLocally("two", named), answerChoiceLocally("cancel", named)]).toEqual([
      { kind: "refused" }, { kind: "refused" },
    ]);
    expect([collidingChoiceEntry("two", named), collidingChoiceEntry("cancel", named)]).toEqual([1, 2]);
    expect(answerChoiceLocally("number one", named)).toEqual({ kind: "selected", candidate: named[0] });
  });
});
