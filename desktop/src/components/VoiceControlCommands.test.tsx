import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import {
  DEFAULT_DESKTOP_SETTINGS,
  type DesktopSettingsDto,
  type VoiceResolvedParamDto,
  type VoiceResultDto,
  type VoiceStatusDto,
  type VoiceTranscriptionDto,
  type VoiceTranscriptionOutcomeDto,
} from "../lib/bridge";
import type { DeckActionResult, DeckRuntimeState } from "../types";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    <div data-testid={`terminal-${agentId}`} role="group" aria-label={`${label} terminal`} />
  ),
}));

import { DeckShell } from "../App";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

/**
 * PRD #802 — the rows that are not navigation.
 *
 * `VoiceControlPanel.test.tsx` beside this one owns the microphone's own state
 * machine: the toggle, the release, the mount reconcile, the cap. This file
 * owns what the newer rows DO once an utterance has resolved, which is a
 * different question and is why it is a different file rather than more cases
 * in that one.
 *
 * Every test here drives the whole surface through {@link DeckShell}, never the
 * panel alone. That is the point rather than convenience: the members these
 * rows need are published by the panel and read by the shell at dispatch time,
 * so a test that rendered the panel by itself would assert a wiring that does
 * not exist in the app.
 */

type ResolveVoice = ReturnType<typeof vi.fn<(utterance: string) => Promise<VoiceResultDto>>>;

interface VoiceControls {
  voiceStatus: ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;
  voiceStart: ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;
  voiceStop: ReturnType<typeof vi.fn<() => Promise<VoiceTranscriptionDto>>>;
  voiceCancel: ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;
}

function status(overrides: Partial<VoiceStatusDto> = {}): VoiceStatusDto {
  return { state: "idle", capturedMs: 0, maxMs: 30_000, capped: false, available: true, backend: "remote", ...overrides };
}

function heard(transcript: string): VoiceTranscriptionDto {
  return {
    outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` },
    transcribeMs: 11,
    backend: "stub",
    audioMs: 900,
  };
}

/**
 * A microphone that delivers one utterance per activation and is then quiet.
 *
 * `deliver()` re-arms it, which is what a test needs to drive a SECOND
 * utterance — the dictation cases are about what the utterance after the first
 * one does, and a stand-in that spoke once could not ask that question.
 */
function microphone(transcripts: string[]): VoiceControls & { deliver: (transcript: string) => void } {
  const queue = [...transcripts];
  let recording = false;
  let ready = false;
  const controls = {
    voiceStart: vi.fn(async () => {
      recording = true;
      ready = queue.length > 0;
      return status({ state: "recording" });
    }),
    voiceStatus: vi.fn(async () => {
      if (recording && ready) {
        ready = false;
        return status({ state: "done", capturedMs: 900 });
      }
      return status({ state: recording ? "recording" : "idle" });
    }),
    voiceStop: vi.fn(async () => {
      recording = false;
      return heard(queue.shift() ?? "");
    }),
    voiceCancel: vi.fn(async () => {
      recording = false;
      return status();
    }),
  };
  return {
    ...controls,
    deliver: (transcript: string) => {
      queue.push(transcript);
      ready = recording;
    },
  };
}

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  };
}

function runtime(resolveVoice: ResolveVoice, voice: VoiceControls, overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const snapshot = createFixtureSnapshot("connected");
  const settings = settingsStore();
  return {
    mode: "fixture",
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    testEndpoint: vi.fn(async (_settings, selection: string) => ({
      endpointId: selection,
      deck: selection,
      state: "ssh_unavailable" as const,
      ok: false,
      message: "No deck is reachable from this test runtime.",
      disclosureKnown: false,
      forwards: [],
      knownHosts: [],
      clientProtocolVersion: 0,
      clientBuildVersion: "test",
    })),
    secretStatus: vi.fn(async () => ({ stored: false })),
    storeSecret: vi.fn(async () => ({ stored: true })),
    forgetSecret: vi.fn(async () => ({ stored: false })),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    resolveVoice,
    ...voice,
    ...overrides,
  } as unknown as DeckRuntimeState;
}

/** The whole outcome shape, so a test states only what it is about. */
function dispatch(action: string, invoke: string, sentence: string, transcript: string, params: VoiceResolvedParamDto[] = []): VoiceResultDto {
  return { resolveMs: 21, backend: "stub", outcome: { kind: "dispatch", transcript, action, invoke, params, sentence } };
}

function voiceButton(): HTMLButtonElement {
  return screen.getByTestId("voice-trigger");
}

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function turnVoiceOn() {
  await flush();
  await act(async () => {
    fireEvent.click(voiceButton());
    await Promise.resolve();
    await Promise.resolve();
  });
}

/** Let one whole utterance complete: poll → stop → resolve → dispatch → listen. */
async function completeUtterance() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS);
  });
  await flush();
}

describe("voice off, as a command", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  /**
   * Scenario: turn voice on, then say "voice off". The row dispatches through
   * the registry into the panel's own published member, the microphone is
   * released, and the button reads off again — with the table's report sentence
   * left in the row saying so.
   */
  it("releases the microphone and turns the button off", async () => {
    const voice = microphone(["voice off"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");

    await completeUtterance();

    expect(resolveVoice).toHaveBeenCalledWith("voice off");
    expect(voice.voiceCancel).toHaveBeenCalled();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Voice control off.");
  });

  /**
   * Scenario: say "voice off" while the agent overview is up — the screen that
   * serves the fewest context members. It still stops, because the member it
   * needs is published by the voice surface rather than by a screen, and the
   * surface is mounted on all three.
   */
  it("stops from the overview, where no deck is mounted", async () => {
    const voice = microphone(["voice off"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(voice.voiceCancel).toHaveBeenCalled();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Voice control off.");
  });

  /**
   * Scenario: stopping is not a navigation, so the report carries no Undo. An
   * Undo here would claim to reverse something no screen recorded, and the way
   * back on is the button the user just watched turn off.
   */
  it("offers no Undo, because nothing moved", async () => {
    const voice = microphone(["voice off"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.queryByRole("button", { name: "Undo" })).toBeNull();
  });

  /**
   * Scenario: the microphone is not reopened after a stop. The pipeline's own
   * loop ends by listening again, and this is the one dispatch that has to
   * break that loop — so the start count stays at the single press.
   */
  it("does not start listening again", async () => {
    const voice = microphone(["voice off"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    expect(voice.voiceStart).toHaveBeenCalledTimes(1);

    await completeUtterance();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 8); });

    expect(voice.voiceStart).toHaveBeenCalledTimes(1);
  });
});
