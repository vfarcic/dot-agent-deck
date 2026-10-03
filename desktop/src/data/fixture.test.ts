/// <reference types="node" />
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { fixtureGroundedCommandText, fixtureVoiceCommands, resolveFixtureVoice } from "./fixture";

const dictationSource = readFileSync(resolve("src-tauri/src/voice/dictation.rs"), "utf8");
const outcomeSource = readFileSync(resolve("src-tauri/src/voice/outcome.rs"), "utf8");
const commandsSource = readFileSync(resolve("src-tauri/src/voice/commands.toml"), "utf8");
const fixtureSource = readFileSync(resolve("src/data/fixture.ts"), "utf8");

function phrases(body: string): string[] {
  return [...body.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}

function rustPhrases(name: string, source = dictationSource): string[] {
  const declaration = source.match(new RegExp(`(?:pub )?const ${name}: \\[&str; \\d+\\] = \\[([\\s\\S]*?)\\];`));
  if (!declaration) throw new Error(`Rust phrase list ${name} was not found`);
  return phrases(declaration[1]);
}

function fixturePhrases(action: string): string[] {
  const rows = [...fixtureSource.matchAll(/phrases:\s*\[([^\]]*)\],\s*action:\s*"([^"]+)"/g)];
  const row = rows.find((match) => match[2] === action);
  if (!row) throw new Error(`fixture phrase list ${action} was not found`);
  return phrases(row[1]);
}

function submitPhrases(): string[] {
  const row = commandsSource.match(/id\s*=\s*"submit_prompt"[\s\S]*?heard_as_whole\s*=\s*\[([\s\S]*?)\]/);
  if (!row) throw new Error("Rust submit_prompt heard_as_whole list was not found");
  return [...new Set([...rustPhrases("SUBMIT_PHRASES"), ...phrases(row[1])])];
}

describe("browser fixture voice in typing mode", () => {
  /** Scenario: a spoken stop with transcription punctuation ends typing mode without putting the command in the agent prompt. */
  it("stops on a punctuated stop command", () => {
    expect(resolveFixtureVoice("stop typing.", "agent", true).outcome).toMatchObject({
      kind: "dispatch", action: "dictation_off", params: [],
    });
  });

  /** Scenario: a polite request to send submits the existing prompt and types no command words into it. */
  it("submits on a polite send command", () => {
    expect(resolveFixtureVoice("okay, send it please", "agent", true).outcome).toMatchObject({
      kind: "dispatch", action: "submit_prompt", params: [],
    });
  });

  /** Scenario: the submit row's spoken alias sends the current prompt while typing mode is active. */
  it("submits on go ahead", () => {
    expect(resolveFixtureVoice("go ahead", "agent", true).outcome).toMatchObject({
      kind: "dispatch", action: "submit_prompt", params: [],
    });
  });

  /** Scenario: a separate final send sentence in typing mode leaves the preceding sentence as the prompt text, with a send instruction for the panel. */
  it.each([
    "What's the weather over there? Send it.",
    "What's the weather over there? Send.",
    "What's the weather over there? Submit.",
    "What's the weather over there? Press enter.",
    "What's the weather over there? OKAY, SEND IT PLEASE!",
  ])("types the prompt and sends for a trailing send sentence: %s", (said) => {
    const outcome = resolveFixtureVoice(said, "agent", true).outcome;
    expect(outcome).toMatchObject({
      kind: "dispatch",
      params: [{ value: "What's the weather over there?" }],
    });
  });

  /** Scenario: each Rust voice-off phrase stops listening in typing mode, including aliases absent from the preview's current list. */
  it("turns voice off for every reserved voice-off phrase", () => {
    const wrong = rustPhrases("VOICE_OFF_PHRASES").filter((phrase) =>
      !matchesAction(phrase, "voice_off", true));
    expect(wrong).toEqual([]);
  });

  /** Scenario: a sentence containing a send phrase or ending in a wider-list phrase is typed whole, while whole-utterance send phrases still submit. */
  it("types a sentence that merely mentions send", () => {
    for (const said of [
      "please send it to the tester later",
      "What's the weather over there? Go ahead.",
      "What's the weather over there? Finished.",
      "What's the weather over there? Enter.",
      "What's the weather over there? End.",
    ]) {
      expect(resolveFixtureVoice(said, "agent", true).outcome).toMatchObject({
        kind: "dispatch", action: "dictate_to_agent", params: [{ value: said }],
      });
    }
    for (const said of ["send it", "go ahead", "finished"]) {
      expect(resolveFixtureVoice(said, "agent", true).outcome).toMatchObject({
        kind: "dispatch", action: "submit_prompt", params: [],
      });
    }
  });
});

function matchesAction(utterance: string, action: string, dictating: boolean): boolean {
  const outcome = resolveFixtureVoice(utterance, "agent", dictating).outcome;
  return outcome.kind === "dispatch" && outcome.action === action && outcome.params.length === 0;
}

describe("browser fixture reserved phrase parity with Rust", () => {
  const lists = [
    ["DICTATION_ON_PHRASES", "dictation_on", false],
    ["DICTATION_OFF_PHRASES", "dictation_off", true],
    ["VOICE_OFF_PHRASES", "voice_off", true],
    ["SUBMIT_PHRASES", "submit_prompt", true],
  ] as const;

  /** Scenario: the preview and Rust carry the same reserved lists, including submit aliases declared on Rust's command row. The failure identifies missing and extra phrases by action. */
  it("has the same reserved phrases as Rust", () => {
    const differences = lists.flatMap(([name, action]) => {
      const expected = name === "SUBMIT_PHRASES" ? submitPhrases() : rustPhrases(name);
      const actual = fixturePhrases(action);
      return [
        ...expected.filter((phrase) => !actual.includes(phrase)).map((phrase) => `${action}: missing ${phrase}`),
        ...actual.filter((phrase) => !expected.includes(phrase)).map((phrase) => `${action}: extra ${phrase}`),
      ];
    });
    expect(differences).toEqual([]);
  });

  /** Scenario: every phrase from Rust's reserved lists dispatches identically in the preview, with each Rust politeness word on either edge. */
  it("classifies every Rust phrase with edge politeness", () => {
    const polite = rustPhrases("WHOLE_UTTERANCE_POLITENESS", outcomeSource);
    const wrong = lists.flatMap(([name, action, dictating]) => {
      const expected = name === "SUBMIT_PHRASES" ? submitPhrases() : rustPhrases(name);
      return expected.flatMap((phrase) => [phrase, ...polite.map((word) => `${word}, ${phrase} ${word}`)]
        .filter((utterance) => !matchesAction(utterance, action, dictating))
        .map((utterance) => `${action}: ${utterance}`));
    });
    expect(wrong).toEqual([]);
  });
});

/** A string field of one `commands.toml` row, read off the file Rust embeds. */
function rowField(id: string, field: string): string {
  const row = commandsSource.split("[[commands]]").find((block) => new RegExp(`^\\s*id\\s*=\\s*"${id}"`, "m").test(block));
  if (!row) throw new Error(`commands.toml row ${id} was not found`);
  const value = row.match(new RegExp(`^${field}\\s*=\\s*"([^"]*)"`, "m"));
  if (!value) throw new Error(`commands.toml row ${id} has no ${field}`);
  return value[1];
}

describe("browser fixture Command row parity with Rust (PR #1451 round 4, D8)", () => {
  /** Scenario: the preview's Command row carries Rust's own invoke, hint and report, so a browser test reads the sentences a live build renders. */
  it("has the same invoke, hint and report as commands.toml", () => {
    expect(fixtureSource).toContain(`invoke: "${rowField("set_new_agent_command", "invoke")}"`);
    expect(fixtureSource).toContain(`unavailableHint: "${rowField("set_new_agent_command", "unavailable_hint")}"`);
    expect(fixtureSource).toContain(`report: "${rowField("set_new_agent_command", "report")}"`);
  });

  /** Scenario: with the New agent form live, the maintainer's sentence sets Command to the three words as said and reports them; with no form it is refused with the row's hint. */
  it("sets the command the user said only while the form is live", () => {
    expect(resolveFixtureVoice("Set the command to devbox run agent.", "overview", false, false, true).outcome).toMatchObject({
      kind: "dispatch",
      action: "set_new_agent_command",
      invoke: "setNewAgentCommand",
      params: [{ name: "command", kind: "command_text", value: "devbox run agent" }],
      sentence: "Command: “devbox run agent”.",
    });
    expect(resolveFixtureVoice("Set the command to devbox run agent.", "overview", false, true, false).outcome).toMatchObject({
      kind: "unavailable",
      sentence: `Not here — ${rowField("set_new_agent_command", "unavailable_hint")}.`,
    });
  });

  /** Scenario: the preview drops what `voice::command_text` drops — surrounding quotes and a sentence's full stop — and keeps everything else as said. */
  it("keeps the command as said, as voice::command_text does", () => {
    const cases: [string, string, string | undefined][] = [
      ["Set the command to devbox run agent.", "devbox run agent.", "devbox run agent"],
      ["Set the command to “npm run dev”.", "“npm run dev”.", "npm run dev"],
      ["set the command to claude --model haiku.", "claude --model haiku.", "claude --model haiku"],
      ["set the command to cd ..", "cd ..", "cd .."],
      ["Set the command to devbox run agent.", "devbox run agent --verbose", undefined],
      ["Set the command to devbox run agent.", "dev", undefined],
      ["set the command", "“”.", undefined],
    ];
    expect(cases.map(([said, value]) => fixtureGroundedCommandText(said, value))).toEqual(cases.map(([, , expected]) => expected));
  });

  /** Scenario: the preview takes a command only as whole spoken tokens, as `voice::command_text` does: "./run.sh" and "--model=haiku" are kept whole, and a part of either is refused. */
  it("keeps whole path and option tokens and refuses a part of one", () => {
    const cases: [string, string, string | undefined][] = [
      ["set the command to ./run.sh", "./run.sh", "./run.sh"],
      ["set the command to ./run.sh", "run.sh", undefined],
      ["set the command to claude --model=haiku", "claude --model=haiku", "claude --model=haiku"],
      ["set the command to claude --model=haiku", "model=haiku", undefined],
      ["set the command to cd ..", "cd .", undefined],
      ["set the command to “npm run dev.”", "npm run dev", "npm run dev"],
    ];
    expect(cases.map(([said, value]) => fixtureGroundedCommandText(said, value))).toEqual(cases.map(([, , expected]) => expected));
  });

  /** Scenario: the browser preview refuses C1 and bidi format controls in a spoken Command, matching the live Rust resolver's control-character boundary. */
  it("refuses C1 and bidi controls in Command", () => {
    expect(["\u0085", "\u202e"].map((control) =>
      fixtureGroundedCommandText(`set the command to echo ${control}safe`, `echo ${control}safe`),
    )).toEqual([undefined, undefined]);
  });
});

describe("browser fixture Filter text parity with Rust", () => {
  /** Scenario: spoken leading joiners remain in the Filter box, while a final sentence stop or comma is removed. The preview reports the same applied value that the live Rust voice path should apply. */
  it.each([
    ["filter .git", ".git"],
    ["filter -tmp", "-tmp"],
    ["filter _build", "_build"],
    ["filter docs.", "docs"],
    ["filter docs,", "docs"],
  ])("keeps spoken filter joiners and strips presentation: %s", (said, applied) => {
    expect(resolveFixtureVoice(said, "overview", false, true).outcome).toMatchObject({
      kind: "dispatch",
      action: "filter_directories",
      params: [{ name: "text", kind: "filter_text", value: applied }],
      sentence: `Filtering by “${applied}”.`,
    });
  });
});

/** Scenario: the preview's scroll rows are callable on the dashboard and refused while the New agent dialog is declared open, as the live table refuses them (issue #1492). */
it("refuses a preview scroll while the New agent dialog is open", () => {
  expect(resolveFixtureVoice("scroll down", "overview").outcome).toMatchObject({ kind: "dispatch", invoke: "scrollDown" });
  expect(resolveFixtureVoice("scroll down", "overview", false, false, false, true).outcome).toMatchObject({ kind: "unavailable", action: "scroll_down" });
  expect(fixtureVoiceCommands("overview", false, false, true).find((command) => command.id === "scroll_up")?.callable).toBe(false);
  expect(fixtureVoiceCommands("overview").find((command) => command.id === "scroll_up")?.callable).toBe(true);
});
