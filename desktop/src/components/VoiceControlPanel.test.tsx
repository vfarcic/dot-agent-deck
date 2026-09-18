import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "../lib/bridge";
import type { DeckActionResult, DeckRuntimeState } from "../types";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    <div data-testid={`terminal-${agentId}`} role="group" aria-label={`${label} terminal`} />
  ),
}));

import { DeckShell } from "../App";

type VoiceBackend = "claude" | "opencode" | "remote" | "stub";

type VoiceOutcome = {
  kind: string;
  sentence: string;
  transcript?: string;
  action?: string;
  invoke?: string;
  params?: Array<Record<string, string>>;
  hint?: string;
  param?: string;
  spoken?: string;
  matches?: string[];
  detail?: string;
};

interface VoiceResult {
  outcome: VoiceOutcome;
  resolveMs: number | null;
  backend: VoiceBackend;
}

type VoiceCaptureState = "idle" | "recording" | "transcribing" | "done" | "failed";

interface VoiceStatus {
  state: VoiceCaptureState;
  capturedMs: number;
  maxMs: number;
  capped: boolean;
  available: boolean;
  backend: "off" | "remote";
}

type VoiceTranscriptionOutcome =
  | { kind: "heard"; transcript: string; sentence: string }
  | { kind: "not_configured"; detail: string; sentence: string }
  | { kind: "failed"; detail: string; sentence: string };

interface VoiceTranscription {
  outcome: VoiceTranscriptionOutcome;
  transcribeMs: number | null;
  backend: string;
  audioMs: number;
}

type ResolveVoice = ReturnType<typeof vi.fn<(utterance: string) => Promise<VoiceResult>>>;
type VoiceStatusCall = ReturnType<typeof vi.fn<() => Promise<VoiceStatus>>>;
type VoiceStart = ReturnType<typeof vi.fn<() => Promise<VoiceStatus>>>;
type VoiceStop = ReturnType<typeof vi.fn<() => Promise<VoiceTranscription>>>;
type VoiceCancel = ReturnType<typeof vi.fn<() => Promise<VoiceStatus>>>;

interface VoiceControls {
  voiceStatus: VoiceStatusCall;
  voiceStart: VoiceStart;
  voiceStop: VoiceStop;
  voiceCancel: VoiceCancel;
}

type VoiceRuntime = DeckRuntimeState & VoiceControls & { resolveVoice: ResolveVoice };

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

function voiceStatus(overrides: Partial<VoiceStatus> = {}): VoiceStatus {
  return {
    state: "idle",
    capturedMs: 0,
    maxMs: 30_000,
    capped: false,
    available: false,
    backend: "off",
    ...overrides,
  };
}

function transcription(outcome: VoiceTranscriptionOutcome): VoiceTranscription {
  return {
    outcome,
    transcribeMs: outcome.kind === "not_configured" ? null : 183,
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

function result(outcome: VoiceOutcome, resolveMs: number | null = 37, backend: VoiceBackend = "stub"): VoiceResult {
  return { outcome, resolveMs, backend };
}

function resolver(answer: VoiceResult): ResolveVoice {
  return vi.fn(async () => answer);
}

function openVoicePanel() {
  const trigger = screen.queryByRole("button", { name: "Voice" });
  if (!trigger) throw new Error("Voice control trigger is missing from the primary surface.");
  fireEvent.click(trigger);
  return screen.getByRole("dialog", { name: "Voice control" });
}

async function submit(panel: HTMLElement, utterance: string, resolveVoice: ResolveVoice) {
  fireEvent.change(within(panel).getByRole("textbox", { name: "Command" }), {
    target: { value: utterance },
  });
  await act(async () => {
    fireEvent.click(within(panel).getByRole("button", { name: "Run command" }));
    await Promise.resolve();
  });
  expect(resolveVoice).toHaveBeenCalledTimes(1);
  expect(resolveVoice.mock.calls[0][0]).toBe(utterance);
}

async function startListening(panel: HTMLElement) {
  const control = await within(panel).findByRole("button", { name: "Start listening" });
  await act(async () => {
    fireEvent.click(control);
    await Promise.resolve();
  });
}

async function stopListening(panel: HTMLElement) {
  const control = await within(panel).findByRole("button", { name: "Stop listening" });
  await act(async () => {
    fireEvent.click(control);
    await Promise.resolve();
  });
}

const DISPATCH = {
  kind: "dispatch",
  transcript: "show me every agent",
  action: "open_overview",
  invoke: "openOverview",
  params: [],
  sentence: "Opening the agent overview.",
};

const OUTCOMES: Array<{ name: string; utterance: string; outcome: VoiceOutcome }> = [
  {
    name: "dispatch",
    utterance: "show me every agent",
    outcome: DISPATCH,
  },
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
    outcome: {
      kind: "no_match",
      transcript: "what time is it?",
      sentence: "Heard: “what time is it?” — no matching action.",
    },
  },
  {
    name: "unknown-action",
    utterance: "launch the missiles",
    outcome: {
      kind: "unknown_action",
      transcript: "launch the missiles",
      action: "launch_missiles",
      sentence: "Heard: “launch the missiles” — no matching action.",
    },
  },
  {
    name: "missing-param",
    utterance: "open it",
    outcome: {
      kind: "param_missing",
      transcript: "open it",
      action: "open_agent",
      param: "agent",
      sentence: "Heard: “open it” — I could not tell which agent you meant.",
    },
  },
  {
    name: "unresolvable-param",
    utterance: "open the deployer",
    outcome: {
      kind: "param_unresolved",
      transcript: "open the deployer",
      action: "open_agent",
      param: "agent",
      spoken: "deployer",
      sentence: "Heard: “open the deployer” — no agent here matches “deployer”.",
    },
  },
  {
    name: "ambiguous-param",
    utterance: "open the tester",
    outcome: {
      kind: "param_ambiguous",
      transcript: "open the tester",
      action: "open_agent",
      param: "agent",
      spoken: "tester",
      matches: ["tester one", "tester two"],
      sentence: "Heard: “open the tester” — “tester” matches more than one agent: tester one, tester two.",
    },
  },
  {
    name: "backend-failure",
    utterance: "open the tester",
    outcome: {
      kind: "resolution_failed",
      transcript: "open the tester",
      detail: "no intent backend is configured",
      sentence: "Heard: “open the tester” — could not work out what to do (no intent backend is configured).",
    },
  },
  {
    name: "transcription-failure",
    utterance: "use the microphone",
    outcome: {
      kind: "transcription_failed",
      detail: "no transcription backend is configured",
      sentence: "Could not turn that into text (no transcription backend is configured).",
    },
  },
];

const TRANSCRIPTION_OUTCOMES: Array<{ name: string; outcome: VoiceTranscriptionOutcome }> = [
  {
    name: "heard",
    outcome: {
      kind: "heard",
      transcript: "show me every agent",
      sentence: "Heard: “show me every agent”.",
    },
  },
  {
    name: "not-configured",
    outcome: {
      kind: "not_configured",
      detail: "voice transcription is off",
      sentence: "Choose a transcription backend in Voice settings to use the microphone.",
    },
  },
  {
    name: "capture-failure",
    outcome: {
      kind: "failed",
      detail: "the microphone did not produce audio",
      sentence: "Could not turn that recording into text (the microphone did not produce audio).",
    },
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

  /**
   * Scenario: on a machine with neither a microphone nor a stored credential,
   * open Voice, type a command and submit it. The typed path calls the resolver
   * and shows the complete sentence it returned.
   */
  it("runs a typed command without a microphone or credential", async () => {
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, "show me every agent", resolveVoice);

    expect(within(panel).getByText("Opening the agent overview.")).toBeVisible();
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
  });

  /**
   * Scenario: open Voice while transcription is deliberately off. Typed input
   * remains available, no microphone control is offered, and the panel does not
   * describe that product choice as a failure or degraded state.
   */
  it("offers typed input without a broken-looking microphone when transcription is off", async () => {
    const voice = voiceControls();
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    const panel = openVoicePanel();

    await waitFor(() => expect(voice.voiceStatus).toHaveBeenCalledWith());
    expect(within(panel).getByRole("textbox", { name: "Command" })).toBeVisible();
    expect(within(panel).queryByRole("button", { name: "Start listening" })).not.toBeInTheDocument();
    expect(panel).not.toHaveTextContent(/not configured|unavailable|failed|error|broken|degraded/i);
  });

  /**
   * Scenario: with remote transcription available, press the microphone once
   * to begin listening and once more to stop. The control toggles state and the
   * transcription service's complete sentence is rendered.
   */
  it("starts and stops listening with two presses", async () => {
    const heard = {
      kind: "heard" as const,
      transcript: "show me every agent",
      sentence: "Heard: “show me every agent”.",
    };
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceStop: vi.fn(async () => transcription(heard)),
    });
    const resolveVoice = vi.fn<(utterance: string) => Promise<VoiceResult>>(() => new Promise(() => {}));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    const panel = openVoicePanel();
    await startListening(panel);

    expect(voice.voiceStart).toHaveBeenCalledWith();
    expect(within(panel).getByRole("button", { name: "Stop listening" })).toBeVisible();

    await stopListening(panel);

    expect(voice.voiceStop).toHaveBeenCalledWith();
    expect(await within(panel).findByText(heard.sentence)).toBeVisible();
  });

  /**
   * Scenario: start recording, then have a status poll report that the device
   * reached its cap and is no longer recording. The surface observes that
   * reply and stops presenting itself as listening without calling stop.
   */
  it("stops showing listening when a status poll observes the recording cap", async () => {
    vi.useFakeTimers();
    const voice = voiceControls({
      voiceStatus: vi.fn()
        .mockResolvedValueOnce(voiceStatus({ available: true, backend: "remote" }))
        .mockResolvedValue(voiceStatus({
          state: "done",
          capturedMs: 30_000,
          capped: true,
          available: true,
          backend: "remote",
        })),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    const panel = openVoicePanel();
    await act(async () => { await Promise.resolve(); });
    await act(async () => {
      fireEvent.click(within(panel).getByRole("button", { name: "Start listening" }));
      await Promise.resolve();
    });
    expect(within(panel).getByRole("button", { name: "Stop listening" })).toBeVisible();

    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });

    expect(voice.voiceStatus.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(within(panel).queryByRole("button", { name: "Stop listening" })).not.toBeInTheDocument();
    expect(within(panel).queryByText("Listening…")).not.toBeInTheDocument();
    expect(voice.voiceStop).not.toHaveBeenCalled();
  });

  /**
   * Scenario: begin listening and close the Voice panel before stopping. The
   * panel cancels capture as it closes so no hidden microphone remains open.
   */
  it("cancels an in-progress recording when the panel closes", async () => {
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    const panel = openVoicePanel();
    await startListening(panel);
    fireEvent.click(within(panel).getByRole("button", { name: "Close voice control" }));

    await waitFor(() => expect(voice.voiceCancel).toHaveBeenCalledWith());
    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();
  });

  /**
   * Scenario: status offered a microphone but the setting changed before the
   * first press, so start rejects with Rust's not-configured sentence. The
   * panel renders that sentence and remains usable instead of crashing.
   */
  it("renders the not-configured sentence when starting capture is refused", async () => {
    const sentence = "Choose a transcription backend in Voice settings to use the microphone.";
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      // Tauri rejects a Rust `Err(String)` as the string itself, not an Error.
      voiceStart: vi.fn(async () => { throw sentence; }),
    });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    const panel = openVoicePanel();
    await startListening(panel);

    expect(await within(panel).findByText(sentence)).toBeVisible();
    expect(within(panel).getByRole("textbox", { name: "Command" })).toBeVisible();
    expect(within(panel).queryByRole("button", { name: "Stop listening" })).not.toBeInTheDocument();
  });

  /**
   * Scenario: stop one recording for each closed transcription outcome. The
   * panel renders that outcome's own complete sentence, including a capture
   * failure distinct from the intent-resolution failure sentence.
   */
  it.each(TRANSCRIPTION_OUTCOMES)("renders the $name transcription outcome's sentence", async ({ outcome }) => {
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceStop: vi.fn(async () => transcription(outcome)),
    });
    const resolveVoice = outcome.kind === "heard"
      ? vi.fn<(utterance: string) => Promise<VoiceResult>>(() => new Promise(() => {}))
      : resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    const panel = openVoicePanel();
    await startListening(panel);
    await stopListening(panel);

    expect(await within(panel).findByText(outcome.sentence)).toBeVisible();
    if (outcome.kind !== "heard") expect(resolveVoice).not.toHaveBeenCalled();
    if (outcome.kind === "failed") {
      expect(panel).not.toHaveTextContent("could not work out what to do");
    }
  });

  /**
   * Scenario: stopping capture returns a heard transcript that maps to the
   * overview command. The transcript enters the same resolver used by typed
   * input, and its resolved sentence and navigation are rendered.
   */
  it("sends a heard transcript through the shared resolve path", async () => {
    const utterance = "show me every agent";
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceStop: vi.fn(async () => transcription({
        kind: "heard",
        transcript: utterance,
        sentence: `Heard: “${utterance}”.`,
      })),
    });
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    const panel = openVoicePanel();
    await startListening(panel);
    await stopListening(panel);

    await waitFor(() => expect(resolveVoice).toHaveBeenCalledWith(utterance));
    expect(await within(panel).findByText(DISPATCH.sentence)).toBeVisible();
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
  });

  /**
   * Scenario: submit one typed command for each closed Rust outcome. The panel
   * displays the outcome's own complete sentence, and only Dispatch changes
   * the visible screen.
   */
  it.each(OUTCOMES)("displays the $name outcome's rendered sentence", async ({ utterance, outcome }) => {
    const resolveVoice = resolver(result(outcome));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, utterance, resolveVoice);

    expect(within(panel).getByText(outcome.sentence)).toBeVisible();
    if (outcome.kind === "dispatch") {
      expect(screen.getByTestId("overview-table-region")).toBeVisible();
    } else {
      expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    }
  });

  /**
   * Scenario: submit a no-match transcript containing punctuation, odd casing
   * and an inner quote. The report's DOM text preserves the Rust-rendered
   * transcript byte for byte instead of normalising or sanitising it again.
   */
  it("shows the no-match transcript verbatim, including punctuation, casing and an inner quote", async () => {
    const utterance = 'Go, BACK to "Deck"?!';
    const sentence = 'Heard: “Go, BACK to "Deck"?!” — no matching action.';
    const resolveVoice = resolver(result({
      kind: "no_match",
      transcript: utterance,
      sentence,
    }));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, utterance, resolveVoice);

    expect(within(panel).getByText(sentence).textContent).toBe(sentence);
  });

  /**
   * Scenario: a slow Claude resolution completes after 4.2 seconds. Its report
   * presents the backend and latency in one piece of visible metadata, so the
   * delay identifies the backend that caused it.
   */
  it("shows backend and latency together", async () => {
    const resolveVoice = resolver(result(
      { kind: "no_match", transcript: "what time is it?", sentence: "No clock command is available." },
      4_200,
      "claude",
    ));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, "what time is it?", resolveVoice);

    expect(within(panel).getByText("claude, 4.2 s")).toBeVisible();
  });

  /**
   * Scenario: a result says no backend call was made. The backend remains
   * visible, but the report invents neither a zero-millisecond nor zero-second
   * measurement.
   */
  it("shows no timing when resolveMs is null", async () => {
    const sentence = "Nothing was sent because the utterance was silent.";
    const resolveVoice = resolver(result(
      { kind: "transcription_failed", detail: "silence", sentence },
      null,
      "stub",
    ));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, "fixture with no backend call", resolveVoice);

    expect(within(panel).getByText(sentence)).toBeVisible();
    expect(panel).toHaveTextContent("stub");
    expect(panel.textContent).not.toMatch(/\b0(?:\.0+)?\s*(?:ms|s)\b/i);
  });

  /**
   * Scenario: dispatch navigation from the deck to the overview, then take the
   * offered Undo action. The deck the user was on returns and the spent undo
   * affordance disappears.
   */
  it("undoes a dispatched navigation back to the previous view", async () => {
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, "show me every agent", resolveVoice);
    expect(screen.getByTestId("overview-table-region")).toBeVisible();

    fireEvent.click(within(panel).getByRole("button", { name: "Undo" }));

    expect(screen.getByTestId("agent-tile-planner")).toBeVisible();
    expect(within(panel).queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  });

  /**
   * Scenario: dispatch navigation and leave its Undo action untouched for one
   * minute. The affordance expires instead of becoming a permanent second
   * navigation control.
   */
  it("expires the navigation undo window", async () => {
    vi.useFakeTimers();
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, "show me every agent", resolveVoice);
    expect(within(panel).getByRole("button", { name: "Undo" })).toBeVisible();

    act(() => vi.advanceTimersByTime(60_000));

    expect(within(panel).queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
  });

  /**
   * Scenario: resolve uniquely identifiable typed and spoken utterances, then
   * close the panel after both reports appear. No localStorage write or stored
   * value contains either transcript after the complete round trips.
   */
  it("never persists a typed or spoken transcript", async () => {
    const typed = 'SENTINEL typed voice transcript, "Mixed CASE"?!';
    const spoken = 'SENTINEL spoken voice transcript, "Other CASE"?!';
    const sentenceFor = (utterance: string) => `Heard: “${utterance}” — no matching action.`;
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const resolveVoice = vi.fn(async (utterance: string) => result({
      kind: "no_match",
      transcript: utterance,
      sentence: sentenceFor(utterance),
    }));
    const voice = voiceControls({
      voiceStatus: vi.fn(async () => voiceStatus({ available: true, backend: "remote" })),
      voiceStop: vi.fn(async () => transcription({
        kind: "heard",
        transcript: spoken,
        sentence: `Heard: “${spoken}”.`,
      })),
    });
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    const panel = openVoicePanel();
    await submit(panel, typed, resolveVoice);
    expect(within(panel).getByText(sentenceFor(typed))).toBeVisible();
    await startListening(panel);
    await stopListening(panel);
    expect(await within(panel).findByText(sentenceFor(spoken))).toBeVisible();
    fireEvent.click(within(panel).getByRole("button", { name: "Close voice control" }));
    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();

    for (const utterance of [typed, spoken]) {
      for (const call of setItem.mock.calls) expect(String(call[1])).not.toContain(utterance);
      expect(JSON.stringify(window.localStorage)).not.toContain(utterance);
    }
  });
});
