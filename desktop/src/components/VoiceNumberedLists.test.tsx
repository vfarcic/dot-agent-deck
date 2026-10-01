import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeckShell } from "../App";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import { VOICE_STATUS_POLL_MS } from "./VoiceControlPanel";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <div data-testid={`terminal-${agentId}`} />,
}));

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

function makeRuntime(voice: ReturnType<typeof microphone>, snapshot = createFixtureSnapshot("docs")) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => ({
    backend: "stub", resolveMs: 21,
    outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` },
  }));
  const runtime = {
    mode: "fixture", desktopFeatures: fixtureDesktopFeatures(), snapshot, fleet: [snapshot],
    terminalData: {}, clearError: vi.fn(), runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined), reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    testEndpoint: vi.fn(async () => { throw new Error("not used"); }),
    secretStatus: vi.fn(async () => ({ stored: false })),
    storeSecret: vi.fn(async () => ({ stored: true })),
    forgetSecret: vi.fn(async () => ({ stored: false })),
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: undefined })),
    saveSettings: vi.fn(async () => structuredClone(DEFAULT_DESKTOP_SETTINGS)),
    resolveVoice, ...voice,
  } as unknown as DeckRuntimeState;
  return { runtime, resolveVoice };
}

async function poll() {
  await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

describe("numbered voice lists through the dashboard", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /** Scenario: with the dashboard's third row displayed, saying “three” opens that agent. The command resolver is never invoked for the bare number. */
  it("resolves a bare number locally without a Commands backend call", async () => {
    const voice = microphone();
    const { runtime, resolveVoice } = makeRuntime(voice);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    await poll();
    expect(resolveVoice).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-pane-overlay").querySelector(".agent-assignment p"))
      .toHaveTextContent("Check the payment API for breaking changes.");
  });

  /** Scenario: the spoken number names both the first displayed position and an agent named orchestrator-1 in another position. The app offers both choices rather than guessing either agent. */
  it("offers a numbered choice when an agent name collides with one", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    snapshot.agents[1] = { ...snapshot.agents[1], displayName: "orchestrator-1" };
    const { runtime, resolveVoice } = makeRuntime(voice, snapshot);
    render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("one");
    await poll();
    const choice = screen.getByRole("dialog", { name: "Which agent?" });
    expect(choice).toHaveTextContent("Plan / architecture");
    expect(choice).toHaveTextContent("orchestrator-1");
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: while a spoken number is being captured, the first dashboard agent disappears and the visible numbers move. The old answer is refused with nothing opened or sent to Commands. */
  it("refuses an answer after its numbered list changes", async () => {
    const voice = microphone();
    const snapshot = createFixtureSnapshot("docs");
    const { runtime, resolveVoice } = makeRuntime(voice, snapshot);
    const view = render(<DeckShell runtime={runtime} initialView={{ kind: "overview" }} />);
    await act(async () => { fireEvent.click(screen.getByTestId("voice-trigger")); });
    voice.deliver("three");
    const changed = { ...snapshot, agents: snapshot.agents.slice(1) };
    view.rerender(<DeckShell runtime={{ ...runtime, snapshot: changed, fleet: [changed] }} initialView={{ kind: "overview" }} />);
    await poll();
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/moved on|changed/i);
    expect(resolveVoice).not.toHaveBeenCalled();
  });
});
