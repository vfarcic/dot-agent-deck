/**
 * PRD #1666 part 2 (G-fix7) — every agent a voice dispatch names is bound to
 * the daemon process that listed it when the utterance was declared, and the
 * dispatch is refused when another process lists that id by the time it runs:
 * a deck restarted while the utterance resolved, which reused the id for an
 * unrelated agent. The Rust half is stubbed at the runtime seam.
 */
import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fixtureDesktopFeatures, type VoiceOutcomeDto, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import { agentKey } from "../lib/agentKey";
import type { DeckRuntimeState } from "../types";
import { VOICE_STATUS_POLL_MS, VoiceControlPanel, type AgentIncarnation } from "./VoiceControlPanel";

function microphone() {
  const queue: string[] = [];
  let recording = false;
  const status = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({
    state, capturedMs: 900, maxMs: 30_000, capped: false, speech: queue.length > 0,
    available: true, backend: "remote",
  });
  return {
    deliver: (text: string) => queue.push(text),
    voiceStart: vi.fn(async () => { recording = true; return status("recording"); }),
    voiceStatus: vi.fn(async () => status(recording && queue.length ? "done" : "recording")),
    voiceStop: vi.fn(async (): Promise<VoiceTranscriptionDto> => {
      recording = false;
      const transcript = queue.shift() ?? "";
      return { outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` }, transcribeMs: 11, backend: "stub", audioMs: 900 };
    }),
    voiceCancel: vi.fn(async () => { recording = false; return status("idle"); }),
  };
}

const STOP: VoiceResultDto = {
  backend: "stub",
  resolveMs: 21,
  outcome: {
    kind: "dispatch", transcript: "stop the tester", action: "stop_agent", invoke: "confirmStopAgent",
    params: [{ name: "agent", kind: "agent_ref", spoken: "tester", value: "a1", label: "tester" }],
    sentence: "Confirm stopping tester — nothing has been stopped yet.",
  },
};

function runtime(voice: ReturnType<typeof microphone>, answer: () => Promise<VoiceResultDto>) {
  return {
    desktopFeatures: fixtureDesktopFeatures(),
    declareVoiceScreen: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined),
    resolveVoice: vi.fn(answer),
    ...voice,
  } as unknown as DeckRuntimeState;
}

async function flush() {
  for (let i = 0; i < 12; i += 1) await act(async () => { await Promise.resolve(); });
}

async function turnOnVoice() {
  await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
  await flush();
}

async function speak(voice: ReturnType<typeof microphone>, words: string) {
  voice.deliver(words);
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await flush();
}

async function waitOut(ms: number) {
  await act(async () => { await vi.advanceTimersByTimeAsync(ms); });
  await flush();
}

describe("a voice dispatch holds to the daemon process its agent was listed by", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /**
   * Scenario: "stop the tester" is declared while daemon process A lists the
   * tester. While it resolves, the deck restarts as process B and lists an
   * unrelated agent under the same id, with no spawn time to tell them apart.
   * Nothing is dispatched and the report says the agent is gone. When A still
   * lists it (control), the stop is dispatched naming process A.
   */
  it.each([
    ["process-b", false],
    ["process-a", true],
  ] as const)("binds a resolved agent to its process, and refuses it once %s lists the id", async (listedBy, dispatched) => {
    const voice = microphone();
    let release!: (answer: VoiceResultDto) => void;
    const rt = runtime(voice, () => new Promise<VoiceResultDto>((resolve) => { release = resolve; }));
    let listed: Record<string, AgentIncarnation> = { [agentKey("local", "a1")]: { process: "process-a" } };
    const onDispatch = vi.fn((_outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }>) => undefined);
    render(<VoiceControlPanel runtime={rt} screen="overview" onDispatch={onDispatch} selectedDeckId="local" agentIncarnations={() => listed} />);
    await turnOnVoice();
    await speak(voice, "stop the tester");

    listed = { [agentKey("local", "a1")]: { process: listedBy } };
    await act(async () => { release(STOP); });
    await flush();

    if (!dispatched) {
      expect(onDispatch).not.toHaveBeenCalled();
      expect(screen.getByTestId("voice-report")).toHaveTextContent("Nothing ran — that agent is gone (the deck restarted). Say it again.");
      return;
    }
    expect(onDispatch).toHaveBeenCalledTimes(1);
    expect(onDispatch.mock.calls[0][0].params[0]).toMatchObject({ kind: "agent_ref", value: "a1", incarnation: "process-a" });
  });
});
