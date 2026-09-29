import { describe, expect, it } from "vitest";
import { answerChoiceLocally } from "./voiceChoice";

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

  /** Scenario: the closed cancel phrases cancel when said on their own, and not inside a command. */
  it("cancels only on a whole cancel phrase", () => {
    for (const said of ["cancel", "never mind", "none of them", "no"]) expect(answerChoiceLocally(said, offered)).toEqual({ kind: "cancelled" });
    expect(answerChoiceLocally("do not cancel the build", offered)).toEqual({ kind: "not_answer" });
  });
});
