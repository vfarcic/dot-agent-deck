import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import {
  DEFAULT_DESKTOP_SETTINGS,
  type DesktopSettingsDto,
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
import {
  NOTHING_DISPATCHED,
  SCREEN_MOVED_ON,
  VOICE_CAP_DISCARDED,
  VOICE_STATUS_POLL_MS,
  VOICE_UNAVAILABLE,
  VOICE_UNDO_WINDOW_MS,
} from "./VoiceControlPanel";

type ResolveVoice = ReturnType<typeof vi.fn<(utterance: string) => Promise<VoiceResultDto>>>;
type VoiceStatusCall = ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;
type VoiceStart = ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;
type VoiceStop = ReturnType<typeof vi.fn<() => Promise<VoiceTranscriptionDto>>>;
type VoiceCancel = ReturnType<typeof vi.fn<() => Promise<VoiceStatusDto>>>;

interface VoiceControls {
  voiceStatus: VoiceStatusCall;
  voiceStart: VoiceStart;
  voiceStop: VoiceStop;
  voiceCancel: VoiceCancel;
}

type VoiceRuntime = DeckRuntimeState & VoiceControls & { resolveVoice: ResolveVoice };

function voiceStatus(overrides: Partial<VoiceStatusDto> = {}): VoiceStatusDto {
  return {
    state: "idle",
    capturedMs: 0,
    maxMs: 30_000,
    capped: false,
    available: false,
    backend: "local",
    ...overrides,
  };
}

function transcription(outcome: VoiceTranscriptionOutcomeDto): VoiceTranscriptionDto {
  // `null` on the two kinds where no backend was called, which is what
  // `voice::handle_audio` sends: a number there would claim a measurement
  // nobody took. `silent` still names the backend that WOULD have answered,
  // because the microphone and the settings are both fine.
  const called = outcome.kind !== "not_configured" && outcome.kind !== "silent";
  return {
    outcome,
    transcribeMs: called ? 183 : null,
    backend: outcome.kind === "not_configured" ? "off" : "remote",
    audioMs: 1_240,
  };
}

function voiceControls(overrides: Partial<VoiceControls> = {}): VoiceControls {
  return {
    voiceStatus: vi.fn(async () => voiceStatus()),
    voiceStart: vi.fn(async () => voiceStatus({ state: "recording", available: true, backend: "remote" })),
    voiceStop: vi.fn(async () => transcription({
      kind: "failed",
      detail: "the microphone returned no test transcription",
      sentence: "Could not turn that recording into text.",
    })),
    voiceCancel: vi.fn(async () => voiceStatus()),
    ...overrides,
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

function runtime(resolveVoice: ResolveVoice, voice: VoiceControls = voiceControls()): VoiceRuntime {
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
  } as VoiceRuntime;
}

function result(
  outcome: VoiceResultDto["outcome"],
  resolveMs: number | null = 37,
  backend: VoiceResultDto["backend"] = "stub",
): VoiceResultDto {
  return { outcome, resolveMs, backend };
}

function resolver(answer: VoiceResultDto): ResolveVoice {
  return vi.fn(async () => answer);
}

function heard(transcript: string): VoiceTranscriptionOutcomeDto {
  return { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` };
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

async function turnVoiceOn(voice: VoiceControls) {
  await flush();
  await act(async () => {
    fireEvent.click(voiceButton());
    await Promise.resolve();
    await Promise.resolve();
  });
  expect(voice.voiceStart).toHaveBeenCalledTimes(1);
  expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
  expect(voiceButton()).toHaveTextContent(/voice\s+on/i);
}

async function completeAutomaticUtterance(voice: VoiceControls) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS);
  });
  expect(voice.voiceStop).toHaveBeenCalledTimes(1);
}

function automaticVoice(outcome: VoiceTranscriptionOutcomeDto, capped = false): VoiceControls {
  const voiceStart = vi.fn(async () => voiceStatus({ state: "recording", available: true, backend: "remote" }));
  let delivered = false;
  return voiceControls({
    voiceStart,
    voiceStatus: vi.fn(async () => {
      if (voiceStart.mock.calls.length === 0) return voiceStatus({ available: true, backend: "remote" });
      if (!delivered) {
        delivered = true;
        return voiceStatus({
          state: "done",
          capturedMs: capped ? 30_000 : 1_240,
          capped,
          available: true,
          backend: "remote",
        });
      }
      return voiceStatus({ state: "recording", available: true, backend: "remote" });
    }),
    voiceStop: vi.fn(async () => transcription(outcome)),
  });
}

const DISPATCH = {
  kind: "dispatch" as const,
  transcript: "show me every agent",
  action: "open_overview",
  invoke: "openOverview",
  params: [],
  sentence: "Opening the agent overview.",
};

const OPEN_SETTINGS_DISPATCH = {
  kind: "dispatch" as const,
  transcript: "open settings",
  action: "open_settings",
  invoke: "openSettings",
  params: [],
  sentence: "Opening settings.",
};

const OUTCOMES: Array<{ name: string; utterance: string; outcome: VoiceResultDto["outcome"] }> = [
  { name: "dispatch", utterance: "show me every agent", outcome: DISPATCH },
  {
    name: "unavailable",
    utterance: "show me every agent",
    outcome: {
      kind: "unavailable",
      transcript: "show me every agent",
      action: "open_overview",
      hint: "the agent overview opens from the deck",
      sentence: "Not here — the agent overview opens from the deck.",
    },
  },
  {
    name: "no-match",
    utterance: "what time is it?",
    outcome: { kind: "no_match", transcript: "what time is it?", sentence: "Heard: “what time is it?” — no matching action." },
  },
  {
    name: "unknown-action",
    utterance: "launch the missiles",
    outcome: { kind: "unknown_action", transcript: "launch the missiles", action: "launch_missiles", sentence: "Heard: “launch the missiles” — no matching action." },
  },
  {
    name: "missing-param",
    utterance: "open it",
    outcome: { kind: "param_missing", transcript: "open it", action: "open_agent", param: "agent", sentence: "Heard: “open it” — I could not tell which agent you meant." },
  },
  {
    name: "unresolvable-param",
    utterance: "open the deployer",
    outcome: { kind: "param_unresolved", transcript: "open the deployer", action: "open_agent", param: "agent", spoken: "deployer", sentence: "Heard: “open the deployer” — no agent here matches “deployer”." },
  },
  {
    name: "ambiguous-param",
    utterance: "open the tester",
    outcome: { kind: "param_ambiguous", transcript: "open the tester", action: "open_agent", param: "agent", spoken: "tester", matches: ["tester one", "tester two"], sentence: "Heard: “open the tester” — “tester” matches more than one agent: tester one, tester two." },
  },
  {
    name: "backend-failure",
    utterance: "open the tester",
    outcome: { kind: "resolution_failed", transcript: "open the tester", detail: "no intent backend is configured", sentence: "Heard: “open the tester” — could not work out what to do (no intent backend is configured)." },
  },
  {
    name: "transcription-failure",
    utterance: "use the microphone",
    outcome: { kind: "transcription_failed", detail: "no transcription backend is configured", sentence: "Could not turn that into text (no transcription backend is configured)." },
  },
];

const TRANSCRIPTION_OUTCOMES: Array<{ name: string; outcome: VoiceTranscriptionOutcomeDto }> = [
  { name: "heard", outcome: heard("show me every agent") },
  {
    name: "not-configured",
    outcome: { kind: "not_configured", detail: "voice transcription is off", sentence: "Choose a transcription backend in Settings → Voice to use the microphone." },
  },
  {
    name: "capture-failure",
    outcome: { kind: "failed", detail: "the microphone did not produce audio", sentence: "Could not turn that recording into text (the microphone did not produce audio)." },
  },
  // A noise ended a segment and there was nothing in it. Neither a failure nor
  // an instruction, and no resolver call — the assertion below that
  // `resolveVoice` was never called is the half that matters here, because a
  // segment of room tone must cost nothing at all.
  {
    name: "silent",
    outcome: { kind: "silent", sentence: "Nothing was said — still listening." },
  },
];

describe("voice control panel", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })));
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  /** Scenario: Voice begins visibly off, one press turns it on, and the next press turns it off. */
  it("toggles continuous voice control on and off from the Voice button", async () => {
    const voice = voiceControls({ voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })) });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    // The first paint has not heard back from the microphone yet, so it claims
    // nothing about it. `Voice off` is an observation from here on, not the
    // default that a replacement panel used to render over a live device.
    expect(voiceButton()).toHaveAttribute("aria-pressed", "mixed");
    await flush();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    await turnVoiceOn(voice);
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
  });

  /** Scenario: activate Voice and inspect the whole surface. No dialog, command textbox or typed-submit button exists. */
  it("offers no text input or intermediate dialog", async () => {
    const voice = voiceControls({ voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })) });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);

    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Command" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Run command" })).not.toBeInTheDocument();
  });

  /** Scenario: press Voice while transcription is off. The report tells the user to visit Settings → Voice. */
  it("explains how to enable voice when transcription is unavailable", async () => {
    const voice = voiceControls();
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(await screen.findByText(/Settings → Voice/i)).toBeVisible();
    expect(voice.voiceStart).not.toHaveBeenCalled();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();
  });

  /**
   * Scenario: render the deck with voice available and inspect the surface's
   * shape. The trigger and the report are the two cells of ONE row, and that row
   * is the single element the agent pane's inert walk is told to skip.
   */
  it("renders the trigger and the report as one row carrying one peer marker", () => {
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)))} />);

    const row = screen.getByTestId("voice-row");
    // Both cells inside it: this is what stops the report being a box that
    // floats over the screen the pane is on. `.voice-row`'s height is what every
    // other full-height surface subtracts, and a child that escaped the row
    // would escape the reservation with it.
    expect(row).toContainElement(screen.getByTestId("voice-trigger"));
    expect(row).toContainElement(screen.getByTestId("voice-report"));
    expect(row).toHaveAttribute("data-modal-peer", "voice");
    // ONE marker, on the row — the exemption narrowed rather than moved. A
    // marker left on a child as well would be a second exempt sibling wherever
    // the walk happened to reach it.
    expect(document.querySelectorAll('[data-modal-peer="voice"]')).toHaveLength(1);
    // The live region is still its own element inside the row, so a screen
    // reader is listening before the first sentence lands.
    expect(screen.getByTestId("voice-report")).toHaveAttribute("aria-live", "polite");
    expect(screen.getByTestId("voice-report")).toHaveAttribute("role", "status");
  });

  /**
   * Scenario: a segment of room tone with a noise in it comes back `silent`.
   * The row says nothing was said, keeps listening, and spends no resolver call
   * — and it never blames the user's microphone.
   */
  it("reports a segment with no speech in it without calling the resolver", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice({ kind: "silent", sentence: "Nothing was said — still listening." });
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    const report = screen.getByTestId("voice-report");
    expect(report).toHaveTextContent("Nothing was said");
    // The two readings the wording exists to avoid: a failure, and a fault in
    // hardware that is working perfectly.
    expect(report).not.toHaveTextContent(/could not/i);
    expect(report).not.toHaveTextContent(/microphone/i);
    expect(resolveVoice).not.toHaveBeenCalled();
    // Still on, and the device was reopened for the next utterance, which is
    // what makes "still listening" true rather than reassuring.
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
    expect(voice.voiceStart.mock.calls.length).toBeGreaterThan(1);
  });

  /**
   * Scenario: press Voice in a runtime that can resolve commands but has no
   * microphone verbs. It reports how to enable capture and remains visibly off.
   */
  it("does not latch on when the runtime has no microphone capture verbs", async () => {
    const withoutCapture: DeckRuntimeState = runtime(resolver(result(DISPATCH)));
    delete withoutCapture.voiceStart;
    delete withoutCapture.voiceStop;
    delete withoutCapture.voiceStatus;
    delete withoutCapture.voiceCancel;
    render(<DeckShell runtime={withoutCapture} />);

    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(await screen.findByText(VOICE_UNAVAILABLE)).toBeVisible();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("Opening the microphone…");
  });

  /** Scenario: open an agent pane and activate Voice beside it. The button stays reachable and no dialog covers the pane. */
  it("keeps the voice control reachable while an agent pane is open", async () => {
    const voice = voiceControls({ voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })) });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(voiceButton().closest("[inert]")).toBeNull();
    await turnVoiceOn(voice);

    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
  });

  /** Scenario: VAD finishes an utterance while Voice is on. It executes and capture starts again without another press. */
  it("resolves one utterance and remains on for the next", async () => {
    vi.useFakeTimers();
    const utterance = "show me every agent";
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(resolveVoice).toHaveBeenCalledWith(utterance);
    expect(screen.getByText(DISPATCH.sentence)).toBeVisible();
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(voice.voiceStart).toHaveBeenCalledTimes(2);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
    expect(voiceButton()).toHaveTextContent(/voice\s+on/i);
  });

  /** Scenario: turn Voice off while it is listening. Capture is cancelled, not transcribed, so no microphone stays open. */
  it("cancels the microphone when Voice is turned off", async () => {
    const voice = voiceControls({ voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })) });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);
    expect(voice.voiceStop).not.toHaveBeenCalled();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
  });

  /** Scenario: an utterance hits the capture cap while Voice is on. Its audio is discarded unheard, the report says so, and listening resumes. */
  it("discards a capped utterance and continues listening", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("show me every agent"), true);
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });

    expect(voice.voiceStop).not.toHaveBeenCalled();
    expect(resolveVoice).not.toHaveBeenCalled();
    expect(screen.getByText(VOICE_CAP_DISCARDED)).toBeVisible();
    expect(voice.voiceStart).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("button", { name: "Send the recording" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Discard it" })).not.toBeInTheDocument();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
  });

  /** Scenario: capture is refused after status looked available. Its sentence appears and Voice returns visibly off. */
  it("renders the capture refusal and returns Voice to off", async () => {
    const sentence = "Choose a transcription backend in Settings → Voice to use the microphone.";
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceStart: vi.fn(async () => { throw sentence; }),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); await Promise.resolve(); });

    expect(await screen.findByText(sentence)).toBeVisible();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByRole("textbox", { name: "Command" })).not.toBeInTheDocument();
  });

  /** Scenario: VAD completes one recording for each transcription result. The report renders its app-provided sentence. */
  it.each(TRANSCRIPTION_OUTCOMES)("renders the $name transcription outcome's sentence", async ({ outcome }) => {
    vi.useFakeTimers();
    const voice = automaticVoice(outcome);
    const resolveVoice = outcome.kind === "heard"
      ? vi.fn<(utterance: string) => Promise<VoiceResultDto>>(() => new Promise(() => {}))
      : resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText(outcome.sentence)).toBeVisible();
    if (outcome.kind !== "heard") expect(resolveVoice).not.toHaveBeenCalled();
  });

  /** Scenario: a spoken utterance resolves to each closed Rust outcome kind. Its sentence is rendered verbatim. */
  it.each(OUTCOMES)("displays the $name outcome's rendered sentence", async ({ utterance, outcome }) => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result(outcome));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText(outcome.sentence)).toBeVisible();
    if (outcome.kind === "dispatch") expect(screen.getByTestId("overview-table-region")).toBeVisible();
    else expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
  });

  /** Scenario: speech contains odd casing, punctuation and an inner quote. A no-match report preserves it byte for byte. */
  it("shows the no-match transcript verbatim, including punctuation, casing and an inner quote", async () => {
    vi.useFakeTimers();
    const utterance = 'Go, BACK to "Deck"?!';
    const sentence = 'Heard: “Go, BACK to "Deck"?!” — no matching action.';
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result({ kind: "no_match", transcript: utterance, sentence }));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText(sentence).textContent).toBe(sentence);
  });

  /** Scenario: a slow Claude result reports its backend and 4.2-second latency together beside the sentence. */
  it("shows backend and latency together", async () => {
    vi.useFakeTimers();
    const utterance = "what time is it?";
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result(
      { kind: "no_match", transcript: utterance, sentence: "No clock command is available." },
      4_200,
      "claude",
    ));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText("claude, 4.2 s")).toBeVisible();
  });

  /** Scenario: no backend call was made. The backend stays visible without an invented zero-duration measurement. */
  it("shows no timing when resolveMs is null", async () => {
    vi.useFakeTimers();
    const utterance = "silence fixture";
    const sentence = "Nothing was sent because the utterance was silent.";
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result({ kind: "transcription_failed", detail: "silence", sentence }, null, "stub"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    const report = screen.getByTestId("voice-report");
    expect(screen.getByText(sentence)).toBeVisible();
    expect(report).toHaveTextContent("stub");
    expect(report.textContent).not.toMatch(/\b0(?:\.0+)?\s*(?:ms|s)\b/i);
  });

  /** Scenario: a spoken settings command opens the ordinary overlay and its transient report confirms the action. */
  it("opens a deck overlay through the shell voice dispatch", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("open settings"));
    const resolveVoice = resolver(result(OPEN_SETTINGS_DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByTestId("settings-panel")).toBeVisible();
    expect(screen.getByText(OPEN_SETTINGS_DISPATCH.sentence)).toBeVisible();
    expect(screen.queryByText(NOTHING_DISPATCHED)).not.toBeInTheDocument();
  });

  /** Scenario: the same overlay command resolves where its host is absent. The report corrects it and nothing opens. */
  it("reports an unavailable overlay dispatch from the overview without throwing", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("open settings"));
    const resolveVoice = resolver(result(OPEN_SETTINGS_DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText(NOTHING_DISPATCHED)).toBeVisible();
    expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument();
  });

  /** Scenario: a voice command navigates to the overview, then its transient Undo returns to the prior deck view. */
  it("undoes a dispatched navigation back to the previous view", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("show me every agent"));
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Undo" }));

    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  });

  /** Scenario: leave navigation Undo untouched for its complete window. The transient affordance expires. */
  it("expires the navigation undo window", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("show me every agent"));
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    expect(screen.getByRole("button", { name: "Undo" })).toBeVisible();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_UNDO_WINDOW_MS); });

    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  });

  /** Scenario: a unique spoken transcript completes and Voice turns off. Storage never receives it, and no typed path exists. */
  it("never persists a spoken transcript and exposes no typed transcript path", async () => {
    vi.useFakeTimers();
    const utterance = 'SENTINEL spoken voice transcript, "Mixed CASE"?!';
    const sentence = `Heard: “${utterance}” — no matching action.`;
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const voice = automaticVoice(heard(utterance));
    const resolveVoice = resolver(result({ kind: "no_match", transcript: utterance, sentence }));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(screen.queryByRole("textbox", { name: "Command" })).not.toBeInTheDocument();
    for (const call of setItem.mock.calls) expect(String(call[1])).not.toContain(utterance);
    expect(JSON.stringify(window.localStorage)).not.toContain(utterance);
  });

  /** Return a resolver whose answer the test controls, to exercise the ordinary multi-second backend window. */
  function deferredResolver() {
    let answer: (value: VoiceResultDto) => void = () => {};
    const resolveVoice = vi.fn<(utterance: string) => Promise<VoiceResultDto>>(
      () => new Promise<VoiceResultDto>((resolve) => { answer = resolve; }),
    );
    return {
      resolveVoice,
      settle: async (value: VoiceResultDto) => {
        await act(async () => { answer(value); await Promise.resolve(); });
      },
    };
  }

  /** Scenario: VAD submits a command, then Voice turns off before resolution. The abandoned answer runs nothing. */
  it("does not dispatch a request after Voice is turned off", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("show me every agent"));
    const { resolveVoice, settle } = deferredResolver();
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    fireEvent.click(voiceButton());
    // The press abandons the pipeline immediately — which is what this test is
    // about — but the button waits for Rust to acknowledge the release before
    // it claims the device is closed.
    expect(voiceButton()).toHaveTextContent(/voice\s+stopping/i);
    await flush();
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
    await settle(result(DISPATCH));

    expect(screen.queryByTestId("overview-table-region")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
  });

  /** Scenario: a spoken command starts on the overview, then the user moves. Nothing runs and the report explains why. */
  it("does not dispatch a request resolved against the screen the user has left", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("open settings"));
    const { resolveVoice, settle } = deferredResolver();
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    fireEvent.click(screen.getByTestId("open-deck"));
    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    await settle(result(OPEN_SETTINGS_DISPATCH));

    expect(screen.queryByTestId("settings-panel")).not.toBeInTheDocument();
    expect(screen.getByText(SCREEN_MOVED_ON)).toBeVisible();
  });

  /**
   * A microphone that outlives the panel in front of it.
   *
   * Every other fake in this file answers from inside the call it is in; this
   * one keeps the state Rust's `CaptureSession` keeps, so a panel can vanish
   * while the device is still open. That is what a hard webview reload, a
   * crashed web-content process or a destroyed window does to a passive-effect
   * cleanup: the cleanup may not run at all, and if it does its asynchronous
   * IPC may never reach Rust.
   *
   * `voiceStart` refuses the way the live command refuses, because that refusal
   * is the whole of why a stale session matters — `CaptureState::accepts_start`
   * takes idle, done and failed and nothing else, so a panel that renders
   * `Voice off` over a live recording cannot get out of it by pressing the
   * button.
   */
  function survivingMicrophone() {
    let state: VoiceStatusDto["state"] = "idle";
    let swallowNextRelease = false;
    let rejectNextRelease = false;
    const live = { available: true, backend: "remote" as const };
    const controls = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ ...live, state })),
      voiceStart: vi.fn(async () => {
        if (state === "recording" || state === "transcribing") {
          throw "cannot start the microphone: a recording is already running";
        }
        state = "recording";
        return voiceStatus({ ...live, state });
      }),
      voiceStop: vi.fn(async () => {
        state = "done";
        return transcription(heard("show me every agent"));
      }),
      voiceCancel: vi.fn(() => {
        if (swallowNextRelease) {
          swallowNextRelease = false;
          // The webview went away mid-call: Rust never hears it, and the
          // promise never settles.
          return new Promise<VoiceStatusDto>(() => {});
        }
        if (rejectNextRelease) {
          rejectNextRelease = false;
          return Promise.reject("the microphone call failed: the device would not let go");
        }
        state = "idle";
        return Promise.resolve(voiceStatus({ ...live, state }));
      }),
    });
    return {
      controls,
      state: () => state,
      /** The next release never reaches Rust, the way a lost webview's does not. */
      loseTheNextRelease: () => { swallowNextRelease = true; },
      /** The next release is refused while Rust continues holding the session. */
      refuseTheNextRelease: () => { rejectNextRelease = true; },
    };
  }

  /**
   * Scenario: voice is on, the webview is lost so the panel's own release never
   * completes, and a replacement panel mounts against the microphone Rust is
   * still holding. The new panel reconciles rather than rendering `Voice off`
   * over a live device, and the next press opens a microphone instead of being
   * refused.
   */
  it("reconciles a replacement panel against the microphone the lost one left open", async () => {
    const mic = survivingMicrophone();
    const first = render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);

    await turnVoiceOn(mic.controls);
    expect(mic.state()).toBe("recording");

    mic.loseTheNextRelease();
    first.unmount();
    await flush();
    expect(mic.state()).toBe("recording");

    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);
    await flush();

    expect(mic.state()).toBe("idle");
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");

    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); await Promise.resolve(); });

    expect(mic.state()).toBe("recording");
    expect(voiceButton()).toHaveTextContent(/voice\s+on/i);
  });

  /**
   * Scenario: a replacement panel finds the recording its predecessor left
   * open, but Rust refuses the reconcile release. The button must keep a
   * not-released presentation instead of claiming the microphone is off.
   */
  it("does not claim Voice off when mount reconciliation cannot release the microphone", async () => {
    const mic = survivingMicrophone();
    const first = render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);

    await turnVoiceOn(mic.controls);
    mic.loseTheNextRelease();
    first.unmount();
    await flush();
    expect(mic.state()).toBe("recording");

    mic.refuseTheNextRelease();
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);
    await flush();

    expect(mic.state()).toBe("recording");
    expect(voiceButton()).not.toHaveTextContent(/voice\s+off/i);
    expect(voiceButton().querySelector("svg.lucide-mic-off")).toBeNull();
    expect(voiceButton()).not.toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: the reconcile above could not release the device, and the user
   * presses Voice. The press must RETRY the release rather than try to start a
   * recording Rust would refuse, and the button must end at off once it works.
   *
   * The recovery half of the case above: a presentation that never resolves is
   * only half a fix, and the press that looks like *turn it on* is the one the
   * user has.
   */
  it("retries the release when the button is pressed after a failed reconcile", async () => {
    const mic = survivingMicrophone();
    const first = render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);

    await turnVoiceOn(mic.controls);
    mic.loseTheNextRelease();
    first.unmount();
    await flush();

    mic.refuseTheNextRelease();
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);
    await flush();
    expect(mic.state()).toBe("recording");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/press voice again/i);

    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();

    // The retry released it, and nothing tried to start a second recording
    // over the one Rust was holding.
    expect(mic.state()).toBe("idle");
    expect(mic.controls.voiceStart).toHaveBeenCalledTimes(1);
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: the mount reconcile's status read fails, so the panel settles to
   * off knowing nothing — and the press that follows finds a session Rust IS
   * holding and cannot release. The press must not fall through to
   * `voiceStart`, which `accepts_start` refuses anyway, and must not leave the
   * button off over a device that is open.
   *
   * The sibling of the mount-reconcile case, on the path that HAS a press to
   * report against — `turnOn` reconciles again for exactly this reason. It used
   * to end at `off` with an error sentence beside it, which is a better answer
   * than silence and is still the wrong word on the only indication the
   * microphone is open.
   */
  it("does not claim Voice off when a press cannot release the held microphone", async () => {
    const refusal = "the microphone call failed: the device would not let go";
    // Rust is already holding a recording no panel in this test ever started —
    // a session that outlived the webview that opened it.
    let firstStatus = true;
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => {
        if (firstStatus) {
          firstStatus = false;
          // The mount reconcile learns nothing, so it settles to off: this is
          // the state the press starts from.
          throw "the status call failed";
        }
        return voiceStatus({ state: "recording", available: true, backend: "local" });
      }),
      voiceStart: vi.fn(async () => { throw "cannot start the microphone: a recording is already running"; }),
      voiceCancel: vi.fn(async () => { throw refusal; }),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);
    await flush();
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);

    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();

    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);
    expect(voice.voiceStart).not.toHaveBeenCalled();
    expect(voiceButton()).not.toHaveTextContent(/voice\s+off/i);
    expect(voiceButton().querySelector("svg.lucide-mic-off")).toBeNull();
    expect(voiceButton()).not.toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(refusal);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/press voice again/i);
  });

  /**
   * Scenario: the panel is freshly mounted and the microphone has not answered
   * yet. The button says it is finding out rather than claiming the device is
   * closed, and settles to off once the answer arrives.
   */
  it("does not claim Voice off before the microphone has answered", async () => {
    const mic = survivingMicrophone();
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), mic.controls)} />);

    expect(voiceButton()).not.toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "mixed");

    await flush();

    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: turn Voice off and hold the release in flight. The button must
   * not say `Voice off` until Rust has acknowledged the device is gone, and a
   * second press meanwhile must not race the release.
   */
  it("does not claim Voice off until the release has completed", async () => {
    let release: (status: VoiceStatusDto) => void = () => {};
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceCancel: vi.fn(() => new Promise<VoiceStatusDto>((resolve) => { release = resolve; })),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });

    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);
    expect(voiceButton()).not.toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");

    // Serialised rather than racing: nothing is started over a device Rust has
    // not let go of, and no second release is fired at it either.
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });
    expect(voice.voiceStart).toHaveBeenCalledTimes(1);
    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);

    await act(async () => { release(voiceStatus()); await Promise.resolve(); });

    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: releasing the microphone is refused. The button keeps saying the
   * device may still be open, the report carries the refusal and says to press
   * again, and pressing again releases it.
   */
  it("keeps a truthful indication and retries when the release is refused", async () => {
    const refusal = "the microphone call failed: the device would not let go";
    let refuse = true;
    let started = false;
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({
        state: started ? "recording" : "idle",
        available: true,
        backend: "remote",
      })),
      voiceStart: vi.fn(async () => {
        started = true;
        return voiceStatus({ state: "recording", available: true, backend: "remote" });
      }),
      voiceCancel: vi.fn(async () => {
        if (refuse) throw refusal;
        started = false;
        return voiceStatus();
      }),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();

    expect(voiceButton()).not.toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(refusal);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(/press voice again/i);

    refuse = false;
    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();

    expect(started).toBe(false);
    expect(voiceButton()).toHaveTextContent(/voice\s+off/i);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "false");
  });
});
