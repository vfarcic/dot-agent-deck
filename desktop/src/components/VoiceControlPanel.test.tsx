import { act, fireEvent, render, screen, within } from "@testing-library/react";
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

type ResolveVoice = ReturnType<typeof vi.fn<(utterance: string) => Promise<VoiceResult>>>;
type VoiceRuntime = DeckRuntimeState & { resolveVoice: ResolveVoice };

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

function runtime(resolveVoice: ResolveVoice): VoiceRuntime {
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
  } as VoiceRuntime;
}

function result(outcome: VoiceOutcome, resolveMs: number | null = 37, backend: VoiceBackend = "stub"): VoiceResult {
  return { outcome, resolveMs, backend };
}

function resolver(answer: VoiceResult): ResolveVoice {
  return vi.fn(async () => answer);
}

function openVoicePanel() {
  const trigger = screen.queryByRole("button", { name: "Voice", exact: true });
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
   * Scenario: type a uniquely identifiable utterance, submit it, then close the
   * panel after the report appears. No localStorage write or stored value
   * contains any part of that transcript after the complete round trip.
   */
  it("never persists a typed transcript", async () => {
    const utterance = 'SENTINEL voice transcript, "Mixed CASE"?!';
    const sentence = `Heard: “${utterance}” — no matching action.`;
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const resolveVoice = resolver(result({ kind: "no_match", transcript: utterance, sentence }));
    render(<DeckShell runtime={runtime(resolveVoice)} />);

    const panel = openVoicePanel();
    await submit(panel, utterance, resolveVoice);
    expect(within(panel).getByText(sentence)).toBeVisible();
    fireEvent.click(within(panel).getByRole("button", { name: "Close voice control" }));
    expect(screen.queryByRole("dialog", { name: "Voice control" })).not.toBeInTheDocument();

    for (const call of setItem.mock.calls) expect(String(call[1])).not.toContain(utterance);
    expect(JSON.stringify(window.localStorage)).not.toContain(utterance);
  });
});
