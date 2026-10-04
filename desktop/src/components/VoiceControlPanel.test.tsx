import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_PROMPT_KEYS } from "../data/fixture";
import {
  DEFAULT_DESKTOP_SETTINGS,
  fixtureDesktopFeatures,
  type DesktopSettingsDto,
  type VoiceResultDto,
  type VoiceStatusDto,
  type VoiceTranscriptionDto,
  type VoiceTranscriptionOutcomeDto,
} from "../lib/bridge";
import type { AgentSession, AgentTypeId, DeckActionResult, DeckRuntimeState } from "../types";

vi.mock("./TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label, onInput }: { agentId: string; label: string; onInput: (data: string) => void }) => (
    <div data-testid={`terminal-${agentId}`} role="group" aria-label={`${label} terminal`}>
      <textarea
        aria-label={`${label} terminal input`}
        onInput={(event) => onInput(event.currentTarget.value)}
        onKeyDown={(event) => { if (event.key === "Enter") onInput("\r"); }}
      />
    </div>
  ),
}));

import { DeckShell as AppDeckShell } from "../App";

/** Existing deck-specific voice cases enter the deck explicitly. */
function DeckShell(props: Parameters<typeof AppDeckShell>[0]) {
  return <AppDeckShell initialView={{ kind: "deck" }} {...props} />;
}
import {
  NOTHING_DISPATCHED,
  SCREEN_MOVED_ON,
  VOICE_CAP_DISCARDED,
  VOICE_JOIN_WINDOW_MS,
  VOICE_STATUS_POLL_MS,
  VOICE_SUBMIT_SETTLE_MS,
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
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
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
      message: "No daemon is reachable from this test runtime.",
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

/** Let a held answer — one that ran nothing — be reported: it waits for the rest of its sentence first (PR #1451). */
async function settleHeldAnswer() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(VOICE_JOIN_WINDOW_MS);
  });
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

/**
 * Several recordings in a row: each `voiceStart` opens one, the next poll
 * reports it done (with `status` merged in), and `voiceStop` answers with that
 * step's outcome. A step with `capped` set is never stopped, only cancelled.
 */
function sequencedVoice(steps: Array<{ outcome: VoiceTranscriptionOutcomeDto; status?: Partial<VoiceStatusDto> }>): VoiceControls {
  const voiceStart = vi.fn(async () => voiceStatus({ state: "recording", available: true, backend: "remote" }));
  let delivered = 0;
  let stopped = 0;
  return voiceControls({
    voiceStart,
    voiceStatus: vi.fn(async () => {
      const opened = voiceStart.mock.calls.length;
      if (opened === 0) return voiceStatus({ available: true, backend: "remote" });
      if (delivered < opened && opened <= steps.length) {
        delivered = opened;
        return voiceStatus({ state: "done", capturedMs: 1_240, available: true, backend: "remote", ...steps[opened - 1].status });
      }
      return voiceStatus({ state: "recording", available: true, backend: "remote" });
    }),
    voiceStop: vi.fn(async () => transcription(steps[Math.min(stopped++, steps.length - 1)].outcome)),
  });
}

/** What `voice::transcribe::NOTHING_HEARD` says, as the crate sends it. */
const SILENT: VoiceTranscriptionOutcomeDto = {
  kind: "silent",
  detail: "only 60 ms of speech inside the loudest 200 ms, where 120 ms is needed — the loudest moment reached 858 against a room at 64, where speech has to reach 192",
  sentence: "I could not make out any words. Say that again; still listening.",
};

const DISPATCH = {
  kind: "dispatch" as const,
  transcript: "show me every agent",
  action: "open_overview",
  invoke: "openOverview",
  params: [],
  sentence: "Opening the agent dashboard.",
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
      hint: "the agent overview opens from the daemon",
      sentence: "Not here — the agent overview opens from the daemon.",
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
  // `silent` is deliberately not here: it renders NOTHING — see "says nothing
  // about a segment with no speech in it".
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

  describe("voice prompt controls", () => {
    beforeEach(() => {
      vi.useFakeTimers();
      window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    });

    const commands = [
      { said: "interrupt", action: "interrupt_agent", invoke: "interruptAgent", missing: "interrupt its turn" },
      { said: "clear the prompt", action: "clear_prompt", invoke: "clearAgentPrompt", missing: "clear its prompt" },
      { said: "scratch that", action: "scratch_that", invoke: "scratchLastDictation", missing: "remove dictated words" },
    ] as const;

    function dispatchPrompt(said: string): VoiceResultDto {
      const command = commands.find((entry) => entry.said === said);
      const [action, invoke] = command ? [command.action, command.invoke]
        : said === "typing on" ? ["dictation_on", "startDictation"]
        : said === "send it" ? ["submit_prompt", "submitAgentPrompt"]
        : ["dictate_to_agent", "dictateToAgent"];
      return result({
        kind: "dispatch", transcript: said, action, invoke,
        params: invoke === "dictateToAgent"
          ? [{ name: "prefix", kind: "spoken_prefix", spoken: "", value: said, label: said }]
          : [],
        // A neutral resolver sentence leaves the panel responsible for reporting
        // whether its terminal operation actually ran, including refusals.
        sentence: "Prompt command pending.",
      }, null, "local");
    }

    async function startPrompt(agentType: AgentTypeId = "codex", overrides: Partial<AgentSession> = {}) {
      const steps: Parameters<typeof sequencedVoice>[0] = [{ outcome: heard("typing on") }];
      const voice = sequencedVoice(steps);
      const resolveVoice = vi.fn(async (said: string) => dispatchPrompt(said));
      const deck = runtime(resolveVoice, voice);
      const snapshot = {
        ...deck.snapshot,
        agents: deck.snapshot.agents.map((agent) => agent.id === "planner" ? {
          ...agent, displayName: "Planner", status: "running" as const,
          writeLease: "write" as const, agentType, turn: "working" as const,
          promptKeys: FIXTURE_PROMPT_KEYS[agentType], spawnedAtMs: 100,
          ...overrides,
        } : agent),
      };
      deck.snapshot = snapshot;
      deck.fleet = [snapshot];
      const view = render(<DeckShell runtime={deck} />);
      fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
      await turnVoiceOn(voice);
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
      await flush();
      expect(screen.getByRole("button", { name: /stop typing/i })).toBeVisible();
      const write = vi.mocked(deck.sendTerminalInput);
      const target = { deckId: snapshot.connection.deckId, agentId: "planner" };
      const say = async (said: string) => {
        const before = voice.voiceStop.mock.calls.length;
        steps.push({ outcome: heard(said) });
        await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
        await flush();
        expect(voice.voiceStop).toHaveBeenCalledTimes(before + 1);
      };
      const keyboard = async (data: string) => {
        const input = within(screen.getByTestId("agent-pane-overlay")).getByRole("textbox", { name: "Planner terminal input" });
        if (data === "\r") fireEvent.keyDown(input, { key: "Enter" });
        else fireEvent.input(input, { target: { value: data } });
        await flush();
        expect(write).toHaveBeenLastCalledWith(target, data);
      };
      const updatePlanner = (change: Partial<AgentSession>) => {
        const next = { ...deck.snapshot, agents: deck.snapshot.agents.map((agent) => agent.id === "planner" ? { ...agent, ...change } : agent) };
        deck.snapshot = next;
        deck.fleet = [next];
        view.rerender(<DeckShell runtime={{ ...deck }} />);
      };
      const changeDeck = () => {
        const next = {
          ...deck.snapshot,
          connection: { ...deck.snapshot.connection, deckId: "deck-second" },
          agents: deck.snapshot.agents.map((agent) => ({ ...agent, daemonId: "deck-second" })),
        };
        view.rerender(<DeckShell runtime={{ ...deck, snapshot: next, fleet: [next] }} />);
      };
      return { ...view, voice, resolveVoice, write, target, say, keyboard, updatePlanner, changeDeck };
    }

    function report() { return screen.getByTestId("voice-report"); }
    function expectNoInterruptByte(write: ReturnType<typeof vi.mocked<DeckRuntimeState["sendTerminalInput"]>>) {
      expect(write.mock.calls.some(([, bytes]) => bytes.includes("\x03")), "Ctrl+C must never be sent").toBe(false);
    }

    /** Scenario: issue each prompt command on an unsupported agent or an old deck. The row names the agent and no terminal write occurs. */
    it.each(commands)("refuses $invoke without promptKeys and names Devin", async (command) => {
      const { say, write } = await startPrompt("devin");
      await say(command.said);
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(`Devin has no voice key to ${command.missing}.`);
    });

    /** Scenario: the daemon reports an unknown agent type without keys. Each refusal uses the pane label to identify the unsupported target. */
    it.each(commands)("refuses $invoke without an agent label using the pane label", async (command) => {
      const { say, write } = await startPrompt("none");
      await say(command.said);
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(`Planner has no voice key to ${command.missing}.`);
    });

    /** Scenario: a prompt command resolves after the user closes its pane. It writes nothing and reports that the context moved on. */
    it.each(commands)("refuses $invoke when its pane closes during resolution", async (command) => {
      const { say, write, resolveVoice } = await startPrompt();
      let release!: (answer: VoiceResultDto) => void;
      resolveVoice.mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }));
      await say(command.said);
      fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
      await act(async () => { release(dispatchPrompt(command.said)); });
      await flush();
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/nothing (ran|was sent)|say it again/i);
      expect(report()).not.toHaveTextContent("Prompt command pending.");
    });

    /** Scenario: a stop confirmation opens while a prompt command is resolving. The confirmation prevents terminal writes and the row explains the refusal. */
    it.each(commands)("refuses $invoke when a confirmation opens during resolution", async (command) => {
      const { say, write, resolveVoice } = await startPrompt();
      let release!: (answer: VoiceResultDto) => void;
      resolveVoice.mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }));
      await say(command.said);
      fireEvent.click(screen.getByTestId("stop-run"));
      expect(screen.getByRole("alertdialog")).toBeVisible();
      await act(async () => { release(dispatchPrompt(command.said)); });
      await flush();
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/confirmation|typing mode.*changed/i);
      expect(report()).toHaveTextContent(/nothing (ran|was sent)|say it again/i);
    });

    /** Scenario: issue a prompt command after its agent is replaced while resolving. The replacement receives no bytes and the row explains the stale context. */
    it.each(commands)("refuses $invoke when the agent is replaced during resolution", async (command) => {
      const { say, write, resolveVoice, updatePlanner } = await startPrompt();
      let release!: (answer: VoiceResultDto) => void;
      resolveVoice.mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }));
      await say(command.said);
      updatePlanner({ spawnedAtMs: 200 });
      await act(async () => { release(dispatchPrompt(command.said)); });
      await flush();
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/replaced|typing mode.*changed/i);
      expect(report()).toHaveTextContent(/nothing (ran|was sent)|say it again/i);
    });

    /** Scenario: the pane becomes unwritable, hides its terminal, changes deck, or leaves typing mode while resolving a prompt command. The stale command writes nothing and reports why it was dropped. */
    it.each(commands.flatMap((command) => ["input blocked", "terminal hidden", "deck changed", "typing stopped"].map((change) => ({ ...command, change }))))(
      "refuses $invoke after $change during resolution",
      async ({ said, change }) => {
        const { say, write, resolveVoice, updatePlanner, changeDeck } = await startPrompt();
        let release!: (answer: VoiceResultDto) => void;
        resolveVoice.mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }));
        await say(said);
        if (change === "input blocked") updatePlanner({ writeLease: "read" });
        if (change === "terminal hidden") fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("tab", { name: "Diff" }));
        if (change === "deck changed") changeDeck();
        if (change === "typing stopped") fireEvent.click(screen.getByRole("button", { name: /stop typing/i }));
        await act(async () => { release(dispatchPrompt(said)); });
        await flush();
        expect(write).not.toHaveBeenCalled();
        expect(report()).toHaveTextContent(/nothing (ran|was sent)|say it again/i);
        expect(report()).not.toHaveTextContent("Prompt command pending.");
      },
    );

    /** Scenario: interrupt an agent whose turn is idle or unreported. No key is sent and the row says the agent is not working. */
    it.each(["idle", undefined] as const)("refuses interrupt when turn is %s", async (turn) => {
      const { say, write } = await startPrompt("codex", { turn });
      await say("interrupt");
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent("Planner is not working on anything.");
    });

    /** Scenario: interrupt each agent with a single verified interrupt step. Exactly that key is written to the visible pane and the row reports success. */
    it.each(["claude_code", "codex", "pi"] as const)("interrupts %s with its verified key", async (agentType) => {
      const { say, write, target } = await startPrompt(agentType);
      await say("interrupt");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS[agentType]!.interrupt[0].bytes]]);
      expectNoInterruptByte(write);
      expect(report()).toHaveTextContent("Interrupted Planner.");
    });

    /** Scenario: interrupt OpenCode with its two-step key. The second write waits for the configured pause before the success row appears. */
    it("honours OpenCode's pause between interrupt writes", async () => {
      const { say, write, target } = await startPrompt("open_code");
      const [first, second] = FIXTURE_PROMPT_KEYS.open_code!.interrupt;
      await say("interrupt");
      expect(write.mock.calls).toEqual([[target, first.bytes]]);
      await act(async () => { await vi.advanceTimersByTimeAsync(first.pauseAfterMs - 1); });
      expect(write).toHaveBeenCalledTimes(1);
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
      await flush();
      expect(write.mock.calls).toEqual([[target, first.bytes], [target, second.bytes]]);
      expectNoInterruptByte(write);
      expect(report()).toHaveTextContent("Interrupted Planner.");
    });

    /** Scenario: repeat interrupt before three seconds have passed while status still says working. The second utterance writes nothing, but a later interrupt is accepted. */
    it("refuses a repeated interrupt within three seconds and allows it afterwards", async () => {
      const { say, write } = await startPrompt();
      await say("interrupt");
      expect(write).toHaveBeenCalledTimes(1);
      write.mockClear();
      await say("interrupt");
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/already interrupted|just interrupted|wait|recent|too soon/i);
      await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
      await say("interrupt");
      expect(write).toHaveBeenCalledTimes(1);
      expect(report()).toHaveTextContent("Interrupted Planner.");
    });

    /** Scenario: clear each per-line editor's prompt. Sixteen verified clear presses reach the pane and the outcome row names the cleared prompt. */
    it.each(["codex", "open_code", "pi"] as const)("clears %s with sixteen per-line presses", async (agentType) => {
      const { say, write, target } = await startPrompt(agentType);
      await say("clear the prompt");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS[agentType]!.clear.bytes.repeat(16)]]);
      expectNoInterruptByte(write);
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
    });

    /** Scenario: clear Claude Code's wrapped editor in bounded writes. The next write waits for the deck-provided pause, and the outcome row confirms completion after both writes. */
    it("clears wrapped rows in writes no larger than maxPressesPerWrite", async () => {
      const { say, write, target } = await startPrompt("claude_code");
      const keys = FIXTURE_PROMPT_KEYS.claude_code!.clear;
      const pauseBetweenWritesMs = (keys as typeof keys & { pauseBetweenWritesMs?: number }).pauseBetweenWritesMs;
      await say("clear the prompt");
      const chunk = keys.bytes.repeat(keys.maxPressesPerWrite!);
      expect(write.mock.calls).toEqual([[target, chunk]]);
      expect(pauseBetweenWritesMs, "Claude's fixture must provide the pause between clear writes").toBeGreaterThan(0);
      await act(async () => { await vi.advanceTimersByTimeAsync(pauseBetweenWritesMs! - 1); });
      expect(write.mock.calls).toEqual([[target, chunk]]);
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
      expect(write.mock.calls).toEqual([[target, chunk], [target, chunk]]);
      expectNoInterruptByte(write);
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
    });

    /** Scenario: dictate two writes and scratch twice. Each scratch deletes only its last write, including the trailing space, and names the removed words in the row. */
    it("scratches the last write including its trailing space then the previous write", async () => {
      const { say, write, target } = await startPrompt();
      await say("first sentence");
      await say("second sentence");
      expect(write.mock.calls).toEqual([[target, "first sentence "], [target, "second sentence "]]);
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS.codex!.deleteChar.bytes.repeat("second sentence ".length)]]);
      expect(report()).toHaveTextContent(/Removed ["“]second sentence ?["”] from Planner's prompt\./);
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS.codex!.deleteChar.bytes.repeat("first sentence ".length)]]);
      expect(report()).toHaveTextContent(/Removed ["“]first sentence ?["”] from Planner's prompt\./);
      expectNoInterruptByte(write);
      write.mockClear();
      await say("scratch that");
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/Nothing to scratch.*\S/i);
    });

    /** Scenario: scratch before any dictation has been written. No bytes reach the pane and the outcome row says there is nothing to remove. */
    it("refuses scratch with empty dictation history", async () => {
      const { say, write } = await startPrompt();
      await say("scratch that");
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/Nothing to scratch.*\S/i);
    });

    /** Scenario: change the prompt after dictation through each input path. Scratch refuses to delete text whose position is no longer known. */
    it.each(["keyboard input", "keyboard send", "voice send", "clear", "interrupt", "agent replaced"])(
      "refuses scratch after %s",
      async (change) => {
        const { say, write, keyboard, updatePlanner } = await startPrompt();
        await say("keep these words");
        expect(write).toHaveBeenLastCalledWith(expect.anything(), "keep these words ");
        if (change === "keyboard input") await keyboard("typed by hand");
        if (change === "keyboard send") await keyboard("\r");
        if (change === "voice send") await say("send it");
        if (change === "clear") await say("clear the prompt");
        if (change === "interrupt") await say("interrupt");
        if (change === "agent replaced") {
          updatePlanner({ spawnedAtMs: 200 });
          await flush();
          await say("typing on");
        }
        write.mockClear();
        await say("scratch that");
        expect(write).not.toHaveBeenCalled();
        expect(report()).toHaveTextContent(/Nothing to scratch.*\S/i);
      },
    );

    /** Scenario: scratch a write whose editor may have collapsed it or whose characters require ambiguous deletion counts. Refuse and explain without sending any deletion key. */
    it.each([
      { name: "over the 800 floor despite Codex's 1000 limit", said: "a".repeat(800), limit: undefined },
      { name: "over a lower agent limit", said: "a".repeat(10), limit: 10 },
      { name: "a combining mark", said: "cafe\u0301", limit: undefined },
      { name: "an astral character", said: "hello \u{1f600}", limit: undefined },
    ])("refuses scratch of $name", async ({ said, limit }) => {
      const keys = structuredClone(FIXTURE_PROMPT_KEYS.codex!);
      if (limit !== undefined) keys.deleteChar.maxLiteralWriteChars = limit;
      const { say, write, target } = await startPrompt("codex", { promptKeys: keys });
      await say(said);
      expect(write).toHaveBeenLastCalledWith(target, `${said} `);
      write.mockClear();
      await say("scratch that");
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/scratch|remove/i);
      expect(report()).toHaveTextContent(/cannot|can't|too long|safely|unsafe|collapsed/i);
    });

    /** Scenario: a write exactly at the eight-hundred-character floor still has a literal deletion count. Scratch accepts it including the final space. */
    it("allows scratch at the 800-character boundary including the trailing space", async () => {
      const { say, write, target } = await startPrompt();
      await say("a".repeat(799));
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS.codex!.deleteChar.bytes.repeat(800)]]);
      expect(report()).toHaveTextContent(/Removed .*from Planner's prompt\./);
    });

    /** Scenario: scratch a short literal voice write in every supported editor. The pane receives its own verified delete-character key for each character including the trailing space. */
    it.each(["claude_code", "codex", "open_code", "pi"] as const)("scratches %s with its verified deletion key", async (agentType) => {
      const { say, write, target } = await startPrompt(agentType);
      await say("a short write");
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS[agentType]!.deleteChar.bytes.repeat("a short write ".length)]]);
      expectNoInterruptByte(write);
      expect(report()).toHaveTextContent(/Removed ["“]a short write ?["”] from Planner's prompt\./);
    });

    /** Scenario: scratch a write exactly at a lower deck-provided literal limit. Counting the trailing space allows the boundary without relaxing that limit. */
    it("allows scratch exactly at a lower maxLiteralWriteChars", async () => {
      const keys = structuredClone(FIXTURE_PROMPT_KEYS.codex!);
      keys.deleteChar.maxLiteralWriteChars = 10;
      const { say, write, target } = await startPrompt("codex", { promptKeys: keys });
      await say("a".repeat(9));
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, keys.deleteChar.bytes.repeat(10)]]);
    });

    /// Scenario: scratch two voice writes that together exceed the literal limit when they landed just before or exactly at the settle boundary, and a pair within the limit. Only the too-close, oversized pair is refused with an explanation and no deletion bytes.
    it.each([
      { name: "the 800-character floor", agentType: "codex" as const, servedLimit: 1000 },
      { name: "a lower deck limit", agentType: "claude_code" as const, servedLimit: 10 },
      { name: "no deck limit", agentType: "claude_code" as const, servedLimit: undefined },
    ].flatMap((limit) => [
      { ...limit, timing: "within settle and over limit", gap: VOICE_SUBMIT_SETTLE_MS - 1, over: true, refused: true },
      { ...limit, timing: "at settle and over limit", gap: VOICE_SUBMIT_SETTLE_MS, over: true, refused: false },
      { ...limit, timing: "within settle and at limit", gap: VOICE_SUBMIT_SETTLE_MS - 1, over: false, refused: false },
    ]))("guards scratch against paste collapse: $timing ($name)", async ({ agentType, servedLimit, gap, over, refused }) => {
      const keys = structuredClone(FIXTURE_PROMPT_KEYS[agentType]!);
      keys.deleteChar.maxLiteralWriteChars = servedLimit;
      const limit = Math.min(800, servedLimit ?? 800);
      const lastLength = Math.floor(limit / 2);
      const first = `${"a".repeat(limit - lastLength + Number(over) - 1)} `;
      const last = `${"b".repeat(lastLength - 1)} `;
      const { say, write, target } = await startPrompt(agentType, { promptKeys: keys });
      await say(first.trimEnd());
      const firstLandedAt = Date.now();
      await act(async () => { await vi.advanceTimersByTimeAsync(gap - VOICE_STATUS_POLL_MS); });
      await say(last.trimEnd());
      expect(Date.now() - firstLandedAt).toBe(gap);
      expect(write.mock.calls).toEqual([[target, first], [target, last]]);
      write.mockClear();
      await say("scratch that");
      if (refused) {
        expect(write).not.toHaveBeenCalled();
        expect(report()).toHaveTextContent(/cannot|can't/i);
        expect(report()).toHaveTextContent(/safely|safe|collapsed|paste|together/i);
      } else {
        expect(write.mock.calls).toEqual([[target, keys.deleteChar.bytes.repeat(last.length)]]);
        expect(report()).toHaveTextContent(/Removed .*from Planner's prompt\./);
      }
    });

    /// Scenario: clear an earlier prompt to establish an empty prompt, then dictate and clear two parts whose combined text exceeds the paste limit. Undo restores each original write one settle apart, and scratch removes exactly the second part including its trailing space.
    it.each(["codex", "claude_code"] as const)("restores clear Undo as separate paced writes then scratches only the last write (%s)", async (agentType) => {
      const { say, write, target } = await startPrompt(agentType);
      await say("earlier cleared prompt");
      await say("clear the prompt");
      if (agentType === "claude_code") {
        const keys = FIXTURE_PROMPT_KEYS.claude_code!.clear;
        const pauseBetweenWritesMs = (keys as typeof keys & { pauseBetweenWritesMs?: number }).pauseBetweenWritesMs;
        expect(pauseBetweenWritesMs, "Claude's fixture must provide the pause between clear writes").toBeGreaterThan(0);
        await act(async () => { await vi.advanceTimersByTimeAsync(pauseBetweenWritesMs!); });
      }
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
      const first = `${"a".repeat(400)} `;
      const second = `${"b".repeat(399)} `;
      await say(first.trimEnd());
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_SUBMIT_SETTLE_MS); });
      await say(second.trimEnd());
      write.mockClear();
      await say("clear the prompt");
      if (agentType === "claude_code") {
        const keys = FIXTURE_PROMPT_KEYS.claude_code!.clear;
        const pauseBetweenWritesMs = (keys as typeof keys & { pauseBetweenWritesMs?: number }).pauseBetweenWritesMs;
        const chunk = keys.bytes.repeat(keys.maxPressesPerWrite!);
        expect(write.mock.calls).toEqual([[target, chunk]]);
        expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
        expect(pauseBetweenWritesMs, "Claude's fixture must provide the pause between clear writes").toBeGreaterThan(0);
        await act(async () => { await vi.advanceTimersByTimeAsync(pauseBetweenWritesMs!); });
        expect(write.mock.calls).toEqual([[target, chunk], [target, chunk]]);
      }
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
      const undo = screen.getByRole("button", { name: "Undo" });
      expect(undo).toBeVisible();
      write.mockClear();
      fireEvent.click(undo);
      await flush();
      expect(write.mock.calls).toEqual([[target, first]]);
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_SUBMIT_SETTLE_MS - 1); });
      expect(write.mock.calls).toEqual([[target, first]]);
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
      expect(write.mock.calls).toEqual([[target, first], [target, second]]);
      expect(report()).toHaveTextContent("Restored Planner's prompt.");
      write.mockClear();
      await say("scratch that");
      expect(write.mock.calls).toEqual([[target, FIXTURE_PROMPT_KEYS[agentType]!.deleteChar.bytes.repeat(second.length)]]);
      expect(report()).toHaveTextContent(/Removed .*from Planner's prompt\./);
    });

    /// Scenario: open a pane whose existing draft is unknown, dictate a voice part, and clear the prompt. Clearing succeeds, but no Undo is offered and the row explains that the whole draft cannot be restored.
    it.each(["codex", "claude_code"] as const)("does not offer clear Undo when the pane's prompt was never seen empty (%s)", async (agentType) => {
      const { say, write, target } = await startPrompt(agentType);
      await say("voice part");
      expect(write).toHaveBeenLastCalledWith(target, "voice part ");
      write.mockClear();
      await say("clear the prompt");
      if (agentType === "claude_code") {
        const keys = FIXTURE_PROMPT_KEYS.claude_code!.clear;
        const pauseBetweenWritesMs = (keys as typeof keys & { pauseBetweenWritesMs?: number }).pauseBetweenWritesMs;
        expect(pauseBetweenWritesMs, "Claude's fixture must provide the pause between clear writes").toBeGreaterThan(0);
        await act(async () => { await vi.advanceTimersByTimeAsync(pauseBetweenWritesMs!); });
      }
      const keys = FIXTURE_PROMPT_KEYS[agentType]!.clear;
      const chunk = keys.bytes.repeat(agentType === "claude_code" ? keys.maxPressesPerWrite! : 16);
      expect(write.mock.calls).toEqual(agentType === "claude_code" ? [[target, chunk], [target, chunk]] : [[target, chunk]]);
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
      expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
      expect(report()).toHaveTextContent(/cannot be undone|can't be undone|cannot undo|can't undo/i);
    });

    /// Scenario: send with keyboard Enter to establish an empty prompt, then type by hand between voice writes and clear. No Undo is offered and the outcome row explains that the text cannot be restored.
    it("does not offer clear Undo after keyboard input in the same pane", async () => {
      const { say, keyboard } = await startPrompt();
      await keyboard("\r");
      await say("voice part");
      await keyboard("hand typed part");
      await say("another voice part");
      await say("clear the prompt");
      expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
      expect(report()).toHaveTextContent("Cleared Planner's prompt.");
      expect(report()).toHaveTextContent(/cannot be undone|can't be undone|cannot undo|can't undo/i);
    });

    /// Scenario: send with keyboard Enter, dictate, and interrupt before clearing the prompt. The interrupt ends knowledge of the voice contents, so the row admits clearing cannot be undone and shows no Undo button.
    it("does not offer clear Undo for voice text predating interrupt", async () => {
      const { say, keyboard } = await startPrompt();
      await keyboard("\r");
      await say("voice part");
      await say("interrupt");
      await say("clear the prompt");
      expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
      expect(report()).toHaveTextContent(/cannot be undone|can't be undone|cannot undo|can't undo/i);
    });

    /// Scenario: clear first to establish an empty prompt, then dictate and clear again. Letting the ten-second Undo window elapse removes the restoration control without typing anything.
    it("expires clear Undo after its existing ten-second window", async () => {
      const { say, write } = await startPrompt();
      await say("clear the prompt");
      await say("voice part");
      await say("clear the prompt");
      expect(screen.getByRole("button", { name: "Undo" })).toBeVisible();
      write.mockClear();
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_UNDO_WINDOW_MS + 1); });
      expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
      expect(write).not.toHaveBeenCalled();
    });

    /// Scenario: send with keyboard Enter to establish an empty prompt, then dictate and clear voice text. Closing the pane before using Undo prevents the stale restoration and reports that the pane changed.
    it("refuses clear Undo after the cleared pane closes", async () => {
      const { say, write, keyboard } = await startPrompt();
      await keyboard("\r");
      await say("voice part");
      await say("clear the prompt");
      const undo = screen.getByRole("button", { name: "Undo" });
      fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
      write.mockClear();
      fireEvent.click(undo);
      await flush();
      expect(write).not.toHaveBeenCalled();
      expect(report()).toHaveTextContent(/nothing (ran|was sent)|pane.*(closed|changed)|screen.*changed/i);
    });
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
   * Scenario: render the daemon with voice available and inspect the surface's
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
   * Scenario: PR #1451's hand test — voice is on, a command has just been
   * reported, and the user types on the keyboard instead of talking. Each burst
   * of keys ends a segment that comes back `silent`. The row says nothing about
   * it: no "Heard …", no refusal with milliseconds and levels in it, the last
   * report stays where it was, and no resolver call is spent.
   */
  it("says nothing about a segment with no speech in it", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([{ outcome: heard("show me every agent") }, { outcome: SILENT }, { outcome: SILENT }]);
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    for (let utterance = 0; utterance < 3; utterance += 1) {
      await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
    }
    expect(voice.voiceStop).toHaveBeenCalledTimes(3);

    const report = screen.getByTestId("voice-report");
    expect(report).toHaveTextContent("Opening the agent dashboard.");
    expect(report).not.toHaveTextContent(SILENT.sentence);
    expect(report).not.toHaveTextContent(/did not hear|make out|ms of speech|loudest/i);
    expect(resolveVoice).toHaveBeenCalledTimes(1);
    // Still on, and the device reopened after every segment.
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
    expect(voice.voiceStart).toHaveBeenCalledTimes(4);
  });

  /**
   * Scenario: the microphone stays open for thirty seconds while the user
   * types and never speaks, so the recording runs to the cap with nobody's
   * speech in it. It is dropped without a word — no "ran to the 30 s limit" —
   * and listening resumes. A capped recording somebody DID speak in still says
   * so (the test below).
   */
  it("drops a capped recording nobody spoke in without a word", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([{ outcome: SILENT, status: { capped: true, capturedMs: 30_000, speech: false } }]);
    const resolveVoice = resolver(result(DISPATCH));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });

    expect(voice.voiceStop).not.toHaveBeenCalled();
    expect(voice.voiceCancel).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(VOICE_CAP_DISCARDED)).not.toBeInTheDocument();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent(/30 s/);
    expect(voice.voiceStart).toHaveBeenCalledTimes(2);
    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
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
    await settleHeldAnswer();

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
    await settleHeldAnswer();

    expect(screen.getByText(sentence).textContent).toBe(sentence);
  });

  /** Scenario: "Devbox run agent." is heard and matches nothing. The report quotes it once, in the no-match sentence, rather than once as heard and again as unmatched. */
  it("reports what it heard once when the answer already quotes it (PR #1451 hand test)", async () => {
    vi.useFakeTimers();
    const utterance = "Devbox run agent.";
    const sentence = `Heard: “${utterance}” — no matching action.`;
    // The capture sentence exactly as Rust's `handle_audio` renders it.
    const voice = automaticVoice({ kind: "heard", transcript: utterance, sentence: `Heard “${utterance}”` });
    const resolveVoice = resolver(result({ kind: "no_match", transcript: utterance, sentence }));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await settleHeldAnswer();

    const report = screen.getByTestId("voice-report").textContent ?? "";
    expect(report.split(utterance).length - 1, report).toBe(1);
    expect(screen.getByText(sentence)).toBeVisible();
  });

  /** Scenario: a command is heard and runs, and its report names the effect rather than the words. What was heard stays on screen beside it, once. */
  it("keeps what it heard beside an answer that does not quote it", async () => {
    vi.useFakeTimers();
    const utterance = "show me every agent";
    const voice = automaticVoice({ kind: "heard", transcript: utterance, sentence: `Heard “${utterance}”.` });
    render(<DeckShell runtime={runtime(resolver(result(DISPATCH)), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    const report = screen.getByTestId("voice-report").textContent ?? "";
    expect(report.split(utterance).length - 1, report).toBe(1);
    expect(screen.getByText(DISPATCH.sentence)).toBeVisible();
  });

  /** Resolves "show me every agent" (however it is punctuated) to the dashboard, and anything else to a no-match. */
  function joiningResolver() {
    return vi.fn<(utterance: string) => Promise<VoiceResultDto>>(async (utterance) => (
      /^show me every agent\.?$/i.test(utterance)
        ? result({ ...DISPATCH, transcript: utterance })
        : result({ kind: "no_match", transcript: utterance, sentence: `Heard: “${utterance}” — no matching action.` })
    ));
  }

  /** Scenario: the user says "Show me", pauses long enough to end the utterance, then says "every agent". The halves are joined and the dashboard opens; "Show me" alone is never reported as matching nothing (PR #1451). */
  it("joins a command that matched nothing with the words that follow a pause", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([
      { outcome: heard("Show me.") },
      { outcome: heard("every agent"), status: { speech: true } },
    ]);
    const resolveVoice = joiningResolver();
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    expect(screen.queryByText("Heard: “Show me.” — no matching action.")).not.toBeInTheDocument();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 2); });

    expect(resolveVoice.mock.calls.map(([utterance]) => utterance)).toEqual(["Show me.", "Show me every agent"]);
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByText(/no matching action/)).not.toBeInTheDocument();
  });

  /** Scenario: the user says something that matches no command and stops. The app waits briefly for more, then says it matched nothing. */
  it("reports a command that matched nothing once nothing follows it", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([{ outcome: heard("Set the command to be.") }]);
    render(<DeckShell runtime={runtime(joiningResolver(), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    const sentence = "Heard: “Set the command to be.” — no matching action.";
    expect(screen.queryByText(sentence)).not.toBeInTheDocument();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_JOIN_WINDOW_MS); });

    expect(screen.getByText(sentence)).toBeVisible();
  });

  /** Scenario: a command that matched nothing is followed by a sound that turns out not to be speech. The app says the command matched nothing rather than waiting on. */
  it("reports a held command when what followed it was not speech", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([
      { outcome: heard("Set the command to be.") },
      { outcome: SILENT, status: { speech: true } },
    ]);
    render(<DeckShell runtime={runtime(joiningResolver(), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 2); });

    expect(voice.voiceStop).toHaveBeenCalledTimes(2);
    expect(screen.getByText("Heard: “Set the command to be.” — no matching action.")).toBeVisible();
  });

  /** Scenario: something the user did not mean as a command is misheard, and straight after it they say a complete command. The command runs; the misheard words do not swallow it (Qodo on PR #1451). */
  it("runs a complete command said straight after words that matched nothing", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([
      { outcome: heard("Thanks for watching.") },
      { outcome: heard("show me every agent"), status: { speech: true } },
    ]);
    const resolveVoice = joiningResolver();
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 2); });

    expect(resolveVoice.mock.calls.map(([utterance]) => utterance)).toEqual(["Thanks for watching.", "Thanks for watching show me every agent", "show me every agent"]);
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByText(/no matching action/)).not.toBeInTheDocument();
  });

  /** Scenario: a command paused in the middle still matches nothing once joined. The whole sentence is reported at once rather than waiting for more (Qodo on PR #1451). */
  it("reports a joined sentence that still matches nothing at once", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([
      { outcome: heard("Set the command to be.") },
      { outcome: heard("devbox run agent"), status: { speech: true } },
    ]);
    render(<DeckShell runtime={runtime(joiningResolver(), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 2); });

    expect(screen.getByText("Heard: “Set the command to be devbox run agent” — no matching action.")).toBeVisible();
  });

  /** Scenario: after a joined sentence that matched nothing, the user goes on speaking. What they say next is worked out on its own, not joined onto the failed sentence (Qodo on PR #1451). */
  it("does not join onto a joined sentence that matched nothing", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([
      { outcome: heard("Set the command to be.") },
      { outcome: heard("devbox run agent"), status: { speech: true } },
      { outcome: heard("show me every agent"), status: { speech: true } },
    ]);
    const resolveVoice = joiningResolver();
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS * 4); });

    expect(resolveVoice.mock.calls.map(([utterance]) => utterance)).toEqual(["Set the command to be.", "Set the command to be devbox run agent", "devbox run agent", "show me every agent"]);
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
  });

  /** Scenario: the user says something that matches no command, then turns voice off before the app has said so. Nothing about it appears afterwards. */
  it("drops a held command when voice is turned off", async () => {
    vi.useFakeTimers();
    const voice = sequencedVoice([{ outcome: heard("Set the command to be.") }]);
    render(<DeckShell runtime={runtime(joiningResolver(), voice)} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);
    await act(async () => { fireEvent.click(voiceButton()); await Promise.resolve(); });
    await settleHeldAnswer();

    expect(screen.queryByText(/no matching action/)).not.toBeInTheDocument();
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
    await settleHeldAnswer();

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
  it("opens a daemon overlay through the shell voice dispatch", async () => {
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

  /** Scenario: a stale resolver names a deck-only drawer action on the overview. The app reports that it cannot dispatch it without throwing or opening a drawer. */
  it("reports an unavailable deck-only dispatch from the overview without throwing", async () => {
    vi.useFakeTimers();
    const voice = automaticVoice(heard("show evidence drawer"));
    const resolveVoice = resolver(result({
      kind: "dispatch",
      transcript: "show evidence drawer",
      action: "show_evidence_drawer",
      invoke: "toggleEvidenceDrawer",
      params: [],
      sentence: "Showing evidence drawer.",
    }));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn(voice);
    await completeAutomaticUtterance(voice);

    expect(screen.getByText(NOTHING_DISPATCHED)).toBeVisible();
    expect(screen.queryByTestId("evidence-drawer")).not.toBeInTheDocument();
  });

  /** Scenario: a voice command navigates to the overview, then its transient Undo returns to the prior daemon view. */
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
