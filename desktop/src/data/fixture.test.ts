/// <reference types="node" />
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { resolveFixtureVoice } from "./fixture";

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
