import { describe, expect, it } from "vitest";
import { answerNumberLocally, numberedEntry, type VoiceNumberedEntryDto, type VoiceNumberedKind, type VoiceNumberedListDto } from "./voiceNumbers";

/*
 * The webview's twin of `voice::numbers::answer`, held to the Rust unit tests'
 * cases for round 4 (D7): a section word with the number, numbers restarting
 * at 1 per section, and the collision rule kept within a section.
 */

const item = (kind: VoiceNumberedKind, label: string): VoiceNumberedEntryDto => ({ kind, value: label.toLowerCase(), label, names: [] });

/** The New agent dialog's three sections: two daemons, four directory rows (`..` first), three Mode chips. */
const DIALOG: VoiceNumberedListDto = {
  generation: 7,
  sections: [
    { kind: "deck", entries: [item("deck", "dev box"), item("deck", "build box")] },
    { kind: "directory", entries: [item("parent", ".."), item("directory", "demo-project"), item("directory", "scratch"), item("directory", "notes")] },
    { kind: "mode", entries: [item("mode", "No mode"), item("mode", "Schedule"), item("mode", "Dispatcher")] },
  ],
};

const agents = (...labels: string[]): VoiceNumberedListDto => ({ generation: 7, sections: [{ kind: "agent", entries: labels.map((label) => item("agent", label)) }] });

describe("answerNumberLocally", () => {
  it.each([
    ["Select directory 3.", "directory", 3],
    ["directory number four", "directory", 4],
    ["open folder three", "directory", 3],
    ["Select daemon 1", "deck", 1],
    ["choose mode 2", "mode", 2],
    ["go to directories 2", "directory", 2],
  ] as const)("answers %s in its section", (said, section, number) => {
    expect(answerNumberLocally(said, DIALOG, 7)).toEqual({ kind: "selected", section, number });
  });

  it("acts on a bare number one section shows and asks where several do", () => {
    expect(answerNumberLocally("four", DIALOG, 7)).toEqual({ kind: "selected", section: "directory", number: 4 });
    expect(answerNumberLocally("three", DIALOG, 7)).toEqual({ kind: "ambiguous", choices: [{ section: "directory", number: 3 }, { section: "mode", number: 3 }] });
    expect(answerNumberLocally("five", DIALOG, 7)).toEqual({ kind: "out_of_range", number: 5, elsewhere: [] });
  });

  it("refuses a section that does not show the number, or numbers nothing", () => {
    expect(answerNumberLocally("select daemon 4", DIALOG, 7)).toEqual({ kind: "out_of_range", section: "deck", number: 4, elsewhere: ["directory"] });
    expect(answerNumberLocally("select directory 99", DIALOG, 7)).toEqual({ kind: "out_of_range", section: "directory", number: 99, elsewhere: [] });
    expect(answerNumberLocally("mode 2", agents("a", "b"), 7)).toEqual({ kind: "not_numbered", section: "mode", shown: ["agent"] });
    expect(answerNumberLocally("directory 3", DIALOG, 8)).toEqual({ kind: "stale" });
  });

  /* Qodo #16 on PR #1451: "zero" said as a word is "number 0", and a number too large to read is past every list; both are refused here, never resolved. */
  it.each(["zero", "number zero", "Number zero.", "option zero", "99999999999999999999"])("refuses %s as a number no item shows", (said) => {
    const answer = answerNumberLocally(said, DIALOG, 7);
    expect(answer).toMatchObject({ kind: "out_of_range", elsewhere: [] });
    expect(answer.kind === "out_of_range" && answer.section).toBeFalsy();
  });

  it("refuses zero in the section it was said with", () => {
    expect(answerNumberLocally("select directory zero", DIALOG, 7)).toEqual({ kind: "out_of_range", section: "directory", number: 0, elsewhere: [] });
  });

  it.each(["open the third one", "select 3", "open docs", "directory scratch"])("leaves %s to the resolver", (said) => {
    expect(answerNumberLocally(said, DIALOG, 7)).toEqual({ kind: "not_number" });
  });

  it("collides after a section word only with a name that is what was said", () => {
    const folders: VoiceNumberedListDto = {
      generation: 7,
      sections: [{ kind: "directory", entries: [item("parent", ".."), ...Array.from({ length: 20 }, (_, at) => item("directory", `folder-${at + 1}`))] }],
    };
    expect(answerNumberLocally("Select directory 13", folders, 7)).toEqual({ kind: "selected", section: "directory", number: 13 });
    const both = { kind: "ambiguous", choices: [{ section: "directory", number: 13 }, { section: "directory", number: 14 }] };
    expect(answerNumberLocally("open folder 13", folders, 7)).toEqual(both);
    expect(answerNumberLocally("thirteen", folders, 7)).toEqual(both);
    expect(answerNumberLocally("one", agents("Plan", "orchestrator-1"), 7)).toEqual({ kind: "ambiguous", choices: [{ section: "agent", number: 1 }, { section: "agent", number: 2 }] });
    expect(answerNumberLocally("agent 1", agents("Plan", "orchestrator-1"), 7)).toEqual({ kind: "selected", section: "agent", number: 1 });
    expect(answerNumberLocally("open agent 1", agents("Plan", "Docs", "agent-1"), 7)).toEqual({ kind: "ambiguous", choices: [{ section: "agent", number: 1 }, { section: "agent", number: 3 }] });
  });

  /* Qodo on PR #1451: a name ending in a number said in two words is that whole number. */
  it("collides a name ending in a compound number with that number only", () => {
    const labels = Array.from({ length: 30 }, (_, at) => `Task ${at + 1}`);
    labels[4] = "worker twenty three";
    labels[6] = "agent twenty-three";
    const heard = agents(...labels);
    const offer = (...numbers: number[]) => ({ kind: "ambiguous", choices: numbers.map((number) => ({ section: "agent", number })) });
    expect(answerNumberLocally("twenty three", heard, 7)).toEqual(offer(23, 5, 7));
    expect(answerNumberLocally("number 23", heard, 7)).toEqual(offer(23, 5, 7));
    expect(answerNumberLocally("three", heard, 7)).toEqual({ kind: "selected", section: "agent", number: 3 });
    expect(answerNumberLocally("agent twenty three", heard, 7)).toEqual(offer(23, 7));
    expect(answerNumberLocally("agent three", heard, 7)).toEqual({ kind: "selected", section: "agent", number: 3 });
  });

  it("finds a numbered item by its section, or the first section showing it", () => {
    expect(numberedEntry(DIALOG, "mode", 3)?.label).toBe("Dispatcher");
    expect(numberedEntry(DIALOG, undefined, 3)?.label).toBe("scratch");
    expect(numberedEntry(DIALOG, "deck", 3)).toBeUndefined();
  });
});
