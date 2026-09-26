import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import {
  DEFAULT_DESKTOP_SETTINGS,
  fixtureDesktopFeatures,
  type DesktopSettingsDto,
  type VoiceCommandDto,
  type VoiceDirectoriesDto,
  type VoiceNewAgentDto,
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

import { DeckShell as AppDeckShell } from "../App";

// The existing command cases cover the experimental deck. The shipped-default
// cases below clear this query parameter in their nested beforeEach.
beforeEach(() => window.history.replaceState({}, "", "/?fixture=1&experimental=1"));

/** Existing deck-specific voice cases enter the deck explicitly. */
function DeckShell(props: Parameters<typeof AppDeckShell>[0]) {
  return <AppDeckShell initialView={{ kind: "deck" }} {...props} />;
}
import { COMMAND_HIDDEN_BY_ORCHESTRATION, DECK_CANNOT_TAKE_AGENT, DECK_NOT_LISTED, DIRECTORY_MOVED_ON, DRAFT_RESTORED, DIRECTORY_NOT_LISTED, FORM_MOVED_ON, MODE_NOT_OFFERED, NO_DIRECTORY_BROWSER, NO_NEW_AGENT_DIALOG, NO_NEW_AGENT_FORM, NO_PARENT_DIRECTORY, spokenName, START_IN_FLIGHT, START_NEEDS_DIRECTORY, STARTING_CLOSE_BLOCKED } from "./NewAgentDialog";
import { CONFIRMATION_ALREADY_OPEN, STOP_BEHIND_NEW_AGENT, STOP_TARGET_GONE } from "./AgentOverview";
import {
  DIALOG_MOVED_ON,
  NOTHING_DISPATCHED,
  VOICE_DICTATION_SEND_MS,
  VOICE_NOTHING_TO_CLOSE,
  VOICE_DICTATION_SUBMIT,
  VOICE_DICTATION_TICK_MS,
  VOICE_STATUS_POLL_MS,
} from "./VoiceControlPanel";

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
  return { state: "idle", capturedMs: 0, maxMs: 30_000, capped: false, speech: false, available: true, backend: "remote", ...overrides };
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
function microphone(transcripts: string[]): VoiceControls & { deliver: (transcript: string) => void; speak: () => void } {
  const queue = [...transcripts];
  let recording = false;
  let ready = false;
  /* Somebody is talking into the open microphone and has not finished. This is
     the state the capture session reports as `speech` WITHOUT `done`, and it is
     the whole of what a pending send is cancelled by. */
  let speaking = false;
  const controls = {
    voiceStart: vi.fn(async () => {
      recording = true;
      speaking = false;
      ready = queue.length > 0;
      return status({ state: "recording" });
    }),
    voiceStatus: vi.fn(async () => {
      if (recording && ready) {
        ready = false;
        return status({ state: "done", capturedMs: 900, speech: true });
      }
      return status({ state: recording ? "recording" : "idle", speech: speaking });
    }),
    voiceStop: vi.fn(async () => {
      recording = false;
      speaking = false;
      return heard(queue.shift() ?? "");
    }),
    voiceCancel: vi.fn(async () => {
      recording = false;
      speaking = false;
      return status();
    }),
  };
  return {
    ...controls,
    deliver: (transcript: string) => {
      queue.push(transcript);
      ready = recording;
    },
    /** Start talking, with no utterance boundary yet — a sentence in progress. */
    speak: () => { speaking = true; },
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
    ...{ desktopFeatures: fixtureDesktopFeatures() },
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

describe("Settings from the overview by voice", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => vi.useRealTimers());

  /**
   * Scenario: while the overview is open, say "open settings". The voice
   * action opens the same Settings sheet that the overview rail exposes.
   */
  it("opens the Settings sheet from the overview", async () => {
    const voice = microphone(["open settings"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("open_settings", "openSettings", "Opening settings.", "open settings"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(resolveVoice).toHaveBeenCalledWith("open settings");
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();
  });
});

describe("what can I say?", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const VOCABULARY: VoiceCommandDto[] = [
    { id: "open_overview", description: "Show every agent in one list.", callable: true, unavailable_hint: "the agent overview opens from the daemon", params: [] },
    { id: "voice_off", description: "Stop listening.", callable: true, unavailable_hint: "turning voice off works anywhere", params: [] },
    { id: "open_deck", description: "Go back to the terminals.", callable: true, unavailable_hint: "returning to the daemon works from the agent overview", params: [] },
    { id: "open_settings", description: "Open Settings.", callable: true, unavailable_hint: "settings open from the rail", params: [] },
    { id: "dictate_to_agent", description: "Type into an agent.", callable: false, unavailable_hint: "open an agent first", params: [] },
  ];

  function listing(voice: VoiceControls, commands = VOCABULARY) {
    const voiceCommands = vi.fn(async () => commands);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("list_commands", "showVoiceCommands", "Here is what you can say.", "what can I say?"));
    return { voiceCommands, runtime: runtime(resolveVoice, voice, { voiceCommands }) };
  }

  /**
   * Scenario: ask what can be said. An overlay opens listing every row the
   * table carries, split by whether this screen can run it — the callable ones
   * under one heading and the rest under another, both generated from the same
   * answer rather than from anything written in the panel.
   */
  it("opens an overlay generated from the table, split by what is callable here", async () => {
    const voice = microphone(["what can I say?"]);
    const { voiceCommands, runtime: deck } = listing(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();

    // No directory browser is declared off the New agent dialog (PRD #1223).
    expect(voiceCommands).toHaveBeenCalledWith("deck", undefined, undefined);
    const overlay = screen.getByTestId("voice-help");
    expect(overlay).toHaveTextContent("Show every agent in one list.");
    const here = overlay.querySelector('[data-where="here"]');
    const elsewhere = overlay.querySelector('[data-where="elsewhere"]');
    expect(Array.from(here?.querySelectorAll("[data-command]") ?? []).map((row) => row.getAttribute("data-command")))
      .toEqual(["open_overview", "voice_off", "open_deck", "open_settings"]);
    expect(Array.from(elsewhere?.querySelectorAll("[data-command]") ?? []).map((row) => row.getAttribute("data-command")))
      .toEqual(["dictate_to_agent"]);
  });

  /**
   * Scenario: the overlay is asked for the screen the user is standing on, not
   * a screen a stale closure remembered. Asked from the overview, it is the
   * overview's answer that is requested.
   */
  it("asks for the screen the user is on", async () => {
    const voice = microphone(["what can I say?"]);
    const { voiceCommands, runtime: deck } = listing(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(voiceCommands).toHaveBeenCalledWith("overview", undefined, undefined);
  });

  /**
   * Scenario: the overlay has two ways out and neither of them is voice, which
   * is what a user who opened it by mistake needs. Close dismisses it; so does
   * Escape.
   */
  it("closes from its own button and from Escape", async () => {
    const voice = microphone(["what can I say?"]);
    const { runtime: deck } = listing(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();

    await act(async () => { fireEvent.click(screen.getByTestId("voice-help-close")); });
    expect(screen.queryByTestId("voice-help")).toBeNull();

    // Asked for again rather than reopened by hand: the second opening has to
    // come down the same path as the first, or this asserts nothing about the
    // dismissal it just performed.
    voice.deliver("what can I say?");
    await completeUtterance();
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();

    await act(async () => { fireEvent.keyDown(window, { key: "Escape" }); });
    expect(screen.queryByTestId("voice-help")).toBeNull();
  });

  /**
   * Scenario: opening the list does not stop voice control. The overlay is the
   * one surface this panel puts over the screen and it must not behave like a
   * dialog that has to be dismissed between utterances — the pipeline goes
   * straight back to listening underneath it.
   */
  it("leaves voice on and listening underneath", async () => {
    const voice = microphone(["what can I say?"]);
    const { runtime: deck } = listing(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(voiceButton()).toHaveAttribute("aria-pressed", "true");
    expect(voice.voiceStart).toHaveBeenCalledTimes(2);
  });

  /**
   * Scenario: a runtime with no vocabulary verb cannot list anything, so the
   * row is refused before it runs rather than opening an overlay that has to
   * explain its own emptiness.
   */
  it("is refused, not opened empty, when the runtime cannot list", async () => {
    const voice = microphone(["what can I say?"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("list_commands", "showVoiceCommands", "Here is what you can say.", "what can I say?"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.queryByTestId("voice-help")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(NOTHING_DISPATCHED);
  });
});

describe("the empty report row", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  /**
   * Scenario: press Voice and read the row before saying anything. It names
   * both ways to stop and the phrase that lists everything — which is the only
   * documentation a user who has just pressed the button will meet.
   */
  it("says how to stop and how to find out what to say", async () => {
    const voice = microphone([]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();

    const hint = screen.getByTestId("voice-hint");
    expect(hint).toHaveTextContent("what can I say?");
    expect(hint).toHaveTextContent("voice off");
    // The non-voice escape, named beside the phrase: if the phrase is misheard
    // the button is the only way out that does not depend on being heard.
    expect(hint).toHaveTextContent(/Voice button/);
  });

  /**
   * Scenario: the row is empty before the press, because "say voice off" over
   * a microphone that is not open would be an instruction for a state the user
   * is not in.
   */
  it("is absent while voice is off", async () => {
    const voice = microphone([]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("voice_off", "stopVoice", "Voice control off.", "voice off"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await flush();

    expect(screen.queryByTestId("voice-hint")).toBeNull();
  });

  /**
   * Scenario: the first utterance replaces it, and a fresh activation brings it
   * back. It is a label for an empty row rather than a first-run tutorial, so
   * it stands whenever the row has nothing else to say — which after a report
   * means the next time the row is cleared.
   */
  it("gives way to a report, and returns when the row is emptied again", async () => {
    const voice = microphone(["show me every agent"]);
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("open_overview", "openOverview", "Opening the agent dashboard.", "show me every agent"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    expect(screen.getByTestId("voice-hint")).toBeInTheDocument();

    await completeUtterance();
    expect(screen.queryByTestId("voice-hint")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening the agent dashboard.");

    // Off and on again: `turnOn` clears the row through `forget()`, which is
    // the user-visible route back to an empty one.
    await act(async () => { fireEvent.click(voiceButton()); });
    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();
    expect(screen.getByTestId("voice-hint")).toBeInTheDocument();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("Opening the agent dashboard.");
  });
});

describe("typing into the open agent", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  /** The connected fixture's own deck id — the composite identity's first half. */
  const DECK_ID = createFixtureSnapshot("connected").connection.deckId ?? "";

  const PLANNER: VoiceResolvedParamDto[] = [
    { name: "agent", kind: "agent_ref", spoken: "planner", value: "planner", label: "Planner" },
  ];
  /** What the daemon itself calls that agent — read from the fixture, not retyped. */
  const PLANNER_LABEL = createFixtureSnapshot("connected").agents.find((agent) => agent.id === "planner")?.displayName ?? "";

  /**
   * One dictation outcome, shaped the way Rust shapes it.
   *
   * **`value` is the text and `spoken` is the boundary**, which is the whole
   * bargain the rebuild rests on: the model may say where the user's words
   * start, and the app takes the words themselves out of its own transcript.
   * So these fixtures carry a `text` that is genuinely a suffix of the
   * `transcript` beside it — a fixture that made one up would be testing a
   * shape the pipeline cannot produce.
   */
  function dictated(transcript: string, prefix: string): VoiceResultDto {
    const text = transcript.slice(prefix.length).trimStart();
    return dispatch("dictate_to_agent", "dictateToAgent", `Typed: “${text}”.`, transcript, [
      { name: "prefix", kind: "spoken_prefix", spoken: prefix, value: text, label: text },
    ]);
  }

  /**
   * A resolver that opens the Planner's pane first and then answers whatever
   * the test asked for.
   *
   * The opening utterance is the shape the product owner asked for — *"open the
   * tester"*, then say what you want typed — and it is a real dispatch rather
   * than a test harness shortcut, because `screens = ["agent"]` means a
   * dictation row cannot be dispatched until a pane is genuinely on screen.
   */
  function speaking(answers: Record<string, VoiceResultDto>): ResolveVoice {
    return vi.fn(async (utterance: string) =>
      utterance === "open the planner"
        ? dispatch("open_agent", "openAgent", "Opening Planner.", utterance, PLANNER)
        : (answers[utterance] ?? dispatch("open_overview", "openOverview", "Opening the agent dashboard.", utterance)));
  }

  async function openPlanner(voice: ReturnType<typeof microphone>) {
    await turnVoiceOn();
    await completeUtterance();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    return voice;
  }

  /**
   * Scenario: open the Planner's pane, then say "type run the login tests". The
   * words land in that agent's own terminal by the path a keystroke takes, and
   * nothing is submitted.
   */
  it("types the words into the open agent's terminal and submits nothing", async () => {
    const voice = microphone(["open the planner"]);
    const deck = runtime(speaking({ "type run the login tests": dictated("type run the login tests", "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver("type run the login tests");
    await completeUtterance();

    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: DECK_ID, agentId: "planner" }, "run the login tests ");
    // Typed, never submitted: no carriage return has been sent.
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: an opener no list could hold. The model marked the boundary, the
   * app verified it against its own transcript, and what is typed is the
   * remainder and nothing else — no "let's write a prompt" in the agent's
   * prompt.
   */
  it("types only the remainder for an opener no list contains", async () => {
    const voice = microphone(["open the planner"]);
    const said = "let's write a prompt run the tests";
    const deck = runtime(speaking({ [said]: dictated(said, "let's write a prompt") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();

    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: DECK_ID, agentId: "planner" }, "run the tests ");
  });

  /**
   * Scenario: the utterance ends in a submit phrase and is typed anyway. This
   * is the one the product owner asked about — a trailing rule would submit
   * *"the meeting is at the"* when somebody said *"type the meeting is at the
   * end"*, and submitting is the last thing that happens to a prompt.
   */
  it("types a trailing submit phrase rather than obeying it", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type hello end";
    const deck = runtime(speaking({ [said]: dictated(said, "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();

    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: DECK_ID, agentId: "planner" }, "hello end ");
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: every utterance still goes through the resolver — there is no
   * mode holding them back — so a command said after a dictated sentence is a
   * command. That is the whole of what the rebuild bought: nothing to exit, so
   * no exit to miss.
   */
  it("keeps resolving every utterance as a command", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type run the login tests";
    const resolveVoice = speaking({ [said]: dictated(said, "type") });
    const deck = runtime(resolveVoice, voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();
    voice.deliver("show me every agent");
    await completeUtterance();

    expect(resolveVoice).toHaveBeenCalledTimes(3);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening the agent dashboard.");
  });

  /**
   * Scenario: after the words are typed the row shows a countdown naming the
   * agent, it runs down a second at a time, and at zero it presses Enter in
   * that agent's prompt. Nothing is sent before the countdown has been on
   * screen for every one of those seconds.
   */
  it("counts down visibly and then submits", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type run the login tests";
    const deck = runtime(speaking({ [said]: dictated(said, "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();

    const line = screen.getByTestId("voice-dictation");
    /* The agent the way the DECK spells it, which with no `agent_ref` in the
       outcome is the open pane's own display name rather than its id. A
       countdown showing an id is where a user would fail to notice they were
       typing into the wrong agent. */
    expect(line).toHaveTextContent(PLANNER_LABEL);
    expect(line).toHaveTextContent("sending in 5 s");

    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_TICK_MS * 2); });
    expect(screen.getByTestId("voice-dictation")).toHaveTextContent("sending in 3 s");
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);

    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_SEND_MS); });
    expect(deck.sendTerminalInput).toHaveBeenLastCalledWith({ deckId: DECK_ID, agentId: "planner" }, VOICE_DICTATION_SUBMIT);
  });

  /**
   * Scenario: keep talking and the pending send is called off. The status poll
   * reports speech while the microphone is open, which is the only signal that
   * arrives DURING a sentence rather than after it — so a long instruction is
   * never cut in half.
   */
  it("cancels the pending send while the user is still speaking", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type can you check";
    const deck = runtime(speaking({ [said]: dictated(said, "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();
    expect(screen.getByTestId("voice-dictation")).toHaveTextContent("sending in 5 s");

    // The user starts talking again two seconds into the countdown.
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_TICK_MS * 2); });
    voice.speak();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_STATUS_POLL_MS); });
    expect(screen.getByTestId("voice-dictation")).not.toHaveTextContent("sending in");

    // And the rest of the five seconds passes with nothing submitted.
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_SEND_MS * 2); });
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: a whole-utterance submit phrase presses Enter at once, without
   * waiting out the countdown — the third way to send, beside the timer and the
   * user's own keyboard.
   */
  it("submits at once when the user says so", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type run the login tests";
    const deck = runtime(speaking({
      [said]: dictated(said, "type"),
      "send it": dispatch("submit_prompt", "submitAgentPrompt", "Sent.", "send it"),
    }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();
    voice.deliver("send it");
    await completeUtterance();

    expect(deck.sendTerminalInput).toHaveBeenLastCalledWith({ deckId: DECK_ID, agentId: "planner" }, VOICE_DICTATION_SUBMIT);
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(2);
    // The countdown is gone, so the timer cannot press Enter a second time
    // into whatever the agent printed in the meantime.
    expect(screen.queryByTestId("voice-dictation")).toBeNull();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_SEND_MS * 2); });
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(2);
  });

  /**
   * Scenario: the non-voice escape, and the one that works when nothing is
   * being heard correctly. Pressing Voice calls off the pending send, so a
   * mis-transcribed sentence is still sitting in an input the user can edit
   * rather than already sent.
   */
  it("the Voice button calls off the pending send", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type run the login tests";
    const deck = runtime(speaking({ [said]: dictated(said, "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();
    expect(screen.getByTestId("voice-dictation")).toHaveTextContent("sending in 5 s");

    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();

    expect(screen.queryByTestId("voice-dictation")).toBeNull();
    await act(async () => { await vi.advanceTimersByTimeAsync(VOICE_DICTATION_SEND_MS * 2); });
    expect(deck.sendTerminalInput).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: a transcript carrying a carriage return would submit the prompt
   * the moment it was written. Every control character becomes a space, so the
   * countdown stays the only thing that can press Enter.
   *
   * **This is the one transformation between the transcript and the agent**,
   * and the test beside it (`types the words into the open agent's terminal`)
   * is what pins that it is identity for everything else.
   */
  it("never lets a transcript submit the prompt by itself", async () => {
    const voice = microphone(["open the planner"]);
    const said = "type run the tests\r\nrm -rf /";
    const deck = runtime(speaking({ [said]: dictated(said, "type") }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();

    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: DECK_ID, agentId: "planner" }, "run the tests rm -rf / ");
  });

  /**
   * Scenario: the refusal that protects the user's words. Rust answers
   * `param_unresolved` when the boundary the model marked is not how the
   * utterance started — nothing is typed, and the row says so rather than
   * typing the model's guess.
   */
  it("types nothing when the marked boundary did not verify", async () => {
    const voice = microphone(["open the planner"]);
    const said = "run the login tests";
    const refusal: VoiceResultDto = {
      resolveMs: 21,
      backend: "stub",
      outcome: {
        kind: "param_unresolved",
        transcript: said,
        action: "dictate_to_agent",
        param: "prefix",
        spoken: "please type",
        sentence: "Heard: “run the login tests” — “please type” is not how that started, so nothing was typed.",
      },
    };
    const deck = runtime(speaking({ [said]: refusal }), voice);
    render(<DeckShell runtime={deck} />);

    await openPlanner(voice);
    voice.deliver(said);
    await completeUtterance();

    expect(deck.sendTerminalInput).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("nothing was typed");
    expect(screen.queryByTestId("voice-dictation")).toBeNull();
  });
});

describe("experimental deck voice gating", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1");
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => vi.useRealTimers());

  /**
   * Scenario: ask for the voice command list on the shipped overview even if
   * an older command table includes open_deck. Only commands for visible screens are offered.
   */
  it("does not offer open_deck when the deck is hidden", async () => {
    const voice = microphone(["what can I say?"]);
    const voiceCommands = vi.fn(async () => [
      { id: "open_overview", description: "Show every agent.", callable: true, unavailable_hint: "", params: [] },
      { id: "open_deck", description: "Show the terminals.", callable: true, unavailable_hint: "", params: [] },
      { id: "open_settings", description: "Open Settings.", callable: true, unavailable_hint: "", params: [] },
    ] as VoiceCommandDto[]);
    const resolveVoice = vi.fn(async () => dispatch("list_commands", "showVoiceCommands", "Here is what you can say.", "what can I say?"));
    render(<DeckShell runtime={runtime(resolveVoice, voice, { voiceCommands })} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    const help = screen.getByTestId("voice-help");
    expect(help.querySelector('[data-command="open_deck"]')).toBeNull();
    expect(help.querySelector('[data-command="open_overview"]')).not.toBeNull();
    expect(help.querySelector('[data-command="open_settings"]')).not.toBeNull();
  });

  /**
   * Scenario: voice resolves a request to open the gated deck while the flag
   * is off. The overview remains on screen, including after the dispatch completes.
   */
  it("refuses a resolved open_deck command while experimental is off", async () => {
    const voice = microphone(["open the deck"]);
    const resolveVoice = vi.fn(async () => dispatch("open_deck", "openDeck", "Opening deck.", "open the deck"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
  });
});

describe("closing what is on top", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const PLANNER: VoiceResolvedParamDto[] = [
    { name: "agent", kind: "agent_ref", spoken: "planner", value: "planner", label: "Planner" },
  ];

  const CLOSE = dispatch("close", "closeTopmost", "Closed.", "close this");

  function closing(): ResolveVoice {
    return vi.fn(async (utterance: string) => {
      if (utterance === "open the planner") return dispatch("open_agent", "openAgent", "Opening Planner.", utterance, PLANNER);
      if (utterance === "open settings") return dispatch("open_settings", "openSettings", "Opening settings.", utterance);
      if (utterance === "what can I say?") return dispatch("list_commands", "showVoiceCommands", "Here is what you can say.", utterance);
      return CLOSE;
    });
  }

  /** A runtime that can actually LIST something, so the overlay opens. */
  function closingDeck(voice: VoiceControls) {
    return runtime(closing(), voice, { voiceCommands: vi.fn(async () => []) });
  }

  /**
   * Scenario: the overlay is opened by voice and closed by voice — the defect
   * this row was added for. Before it, the list could only be dismissed by a
   * click or Escape, which breaks the premise of a hands-free surface.
   */
  it("dismisses the discovery overlay", async () => {
    const voice = microphone(["what can I say?"]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();

    expect(screen.queryByTestId("voice-help")).toBeNull();
  });

  /**
   * Scenario: on the shipped overview, Settings is open beneath the spoken
   * command list. Saying "close" dismisses the list first; if an agent pane
   * then opens over Settings, the pane closes before Settings does.
   */
  it("closes the voice overlay before Settings on the overview", async () => {
    window.history.replaceState({}, "", "/?fixture=1");
    const voice = microphone([]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("what can I say?");
    await completeUtterance();
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByTestId("voice-help")).toBeNull();
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("open the planner");
    await completeUtterance();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByTestId("voice-report")?.textContent ?? "").not.toContain(VOICE_NOTHING_TO_CLOSE);
  });

  /**
   * Scenario: with the experimental deck shown, saying "close" while its
   * Settings sheet is open dismisses the sheet and leaves the deck visible.
   */
  it("closes Settings on the deck", async () => {
    const voice = microphone([]);
    render(<DeckShell runtime={closingDeck(voice)} />);

    await turnVoiceOn();
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("close this");
    await completeUtterance();

    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
    expect(screen.getByRole("button", { name: "Open Planner agent" })).toBeVisible();
    expect(screen.queryByTestId("voice-report")?.textContent ?? "").not.toContain(VOICE_NOTHING_TO_CLOSE);
  });

  /**
   * Scenario: with no overlay up, the same word closes an agent pane opened
   * from the shipped overview. The user returns to that overview without
   * passing through the hidden deck.
   */
  it("closes the agent view when no overlay is up", async () => {
    window.history.replaceState({}, "", "/?fixture=1");
    const voice = microphone(["open the planner"]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();

    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
    expect(screen.queryByTestId("agent-tile-planner")).not.toBeInTheDocument();
  });

  /**
   * Scenario: the overlay wins over the pane, which is the ordering the whole
   * row is about — closing the pane underneath an open overlay would leave the
   * thing the user was looking at still on screen.
   */
  it("takes the overlay before the pane, and then the pane", async () => {
    const voice = microphone(["open the planner"]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();
    voice.deliver("what can I say?");
    await completeUtterance();
    expect(screen.getByTestId("voice-help")).toBeInTheDocument();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByTestId("voice-help")).toBeNull();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
  });

  /** A runtime that can also run the New agent flow, over the one-deck fixture, with the start held until the test settles it. */
  function newAgentDeck(voice: VoiceControls) {
    let settle!: () => void;
    const runAction = vi.fn((action: { type: string }) => action.type === "start_agent"
      ? new Promise<DeckActionResult>((_resolve, reject) => { settle = () => reject(new Error("the daemon refused the start")); })
      : Promise.resolve({ ok: true } as DeckActionResult));
    const deck = runtime(closing(), voice, {
      runAction,
      listDirectories: vi.fn(async () => ({ kind: "listing" as const, path: "/home/dev", displayPath: "/home/dev", entries: [], truncated: false })),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: [] })),
    });
    return { deck, refuseStart: () => settle() };
  }

  /**
   * Scenario: open the New agent dialog on the overview, then open Settings by
   * voice behind it. Saying "close" dismisses the New agent dialog before
   * Settings, then dismisses Settings on the next utterance.
   */
  it("closes the New agent dialog", async () => {
    const voice = microphone([]);
    const { deck } = newAgentDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();

    voice.deliver("open settings");
    await completeUtterance();
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();
    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();

    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeVisible();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
    expect(screen.queryByTestId("voice-report")?.textContent ?? "").not.toContain(VOICE_NOTHING_TO_CLOSE);
  });

  /**
   * Scenario (PRD #1223 U5 and audit F5): with a start in flight, "close" is
   * refused exactly as the X, Esc and the backdrop are — the dialog stays —
   * and the report says why in the dialog's own sentence rather than doing
   * nothing silently. Once the daemon has refused the start, "close" works.
   */
  it("refuses to close the New agent dialog while a start is in flight, and says why", async () => {
    const voice = microphone([]);
    const { deck, refuseStart } = newAgentDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.keyDown(screen.getByTestId("new-agent-deck-list"), { key: "Enter" });
    await flush();
    fireEvent.keyDown(screen.getByTestId("new-agent-directory-list"), { key: " " });
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await flush();
    expect(screen.getByTestId("new-agent-starting")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();
    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(STARTING_CLOSE_BLOCKED);

    await act(async () => refuseStart());
    await flush();
    expect(screen.getByTestId("new-agent-error")).toBeInTheDocument();
    voice.deliver("close this");
    await completeUtterance();
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
  });

  /**
   * Scenario: nothing is on top, so the row reports that rather than claiming a
   * close. The row is callable everywhere — the overlay can be up on any screen
   * — so Rust cannot render a not-here refusal for it, and the honest answer is
   * the surface's own sentence.
   */
  it("says so when there is nothing to close", async () => {
    const voice = microphone(["close this"]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(VOICE_NOTHING_TO_CLOSE);
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
  });
});

describe("opening the New agent dialog, as a command (PRD #1223)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  /* What Rust resolved "the build box" to, against the observed fleet: the
     remote deck's `deckId`, labelled the way the overview labels it. */
  const BUILD_BOX: VoiceResolvedParamDto[] = [
    { name: "deck", kind: "deck_ref", spoken: "the build box", value: FIXTURE_REMOTE_DAEMON_ID, label: "deploy@build-box" },
  ];

  function newAgentFleet(voice: VoiceControls) {
    const fleet = createFixtureFleet("fleet");
    const resolveVoice: ResolveVoice = vi.fn(async (utterance: string) => utterance === "new agent on the build box"
      ? dispatch("open_new_agent", "openNewAgent", "Opening the New agent dialog.", utterance, BUILD_BOX)
      : dispatch("open_new_agent", "openNewAgent", "Opening the New agent dialog.", utterance));
    return runtime(resolveVoice, voice, {
      snapshot: fleet[0],
      fleet,
      listDirectories: vi.fn(async () => ({ kind: "listing" as const, path: "/home/dev", displayPath: "/home/dev", entries: [], truncated: false })),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: [] })),
    });
  }

  const highlightedDeck = () => screen.getByTestId("new-agent-deck-list").querySelector("[aria-selected='true']")?.getAttribute("data-deck-id");

  /**
   * Scenario: on the overview, say "new agent on the build box". The New
   * agent dialog opens with that daemon already chosen — the same state its
   * group header's own New agent button produces — and starts nothing.
   */
  it("opens the dialog with the named deck preselected", async () => {
    const voice = microphone(["new agent on the build box"]);
    const deck = newAgentFleet(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(highlightedDeck()).toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening the New agent dialog.");
    expect(deck.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: say just "new agent". The dialog opens on its deck step with
   * nothing preselected — NOT on whichever deck happens to be selected, which
   * is what a dispatch target's fallback `deckId` would have chosen.
   */
  it("preselects nothing when no deck was named", async () => {
    const voice = microphone(["new agent"]);
    const deck = newAgentFleet(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);

    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(highlightedDeck()).toBeUndefined();
    expect(deck.runAction).not.toHaveBeenCalled();
  });
});

describe("the New agent directory browser, by voice (PRD #1223)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  /* The deck's filesystem, one level per request, as `listDirectories`
     answers it: home has a parent and two children; `billing` has none; the
     filesystem root has no parent. */
  const LEVELS: Record<string, { parent?: string; children: string[] }> = {
    "/home/dev": { parent: "/home", children: ["billing", "docs"] },
    "/home/dev/billing": { parent: "/home/dev", children: [] },
    "/home": { parent: "/", children: ["dev"] },
    "/": { children: ["home"] },
  };
  function listing(path: string) {
    const level = LEVELS[path];
    return {
      kind: "listing" as const,
      path,
      displayPath: path,
      ...(level.parent === undefined ? {} : { parent: level.parent }),
      entries: level.children.map((name) => ({ path: path === "/" ? `/${name}` : `${path}/${name}`, displayName: name, isProject: false })),
      truncated: false,
    };
  }

  /**
   * A resolver that does what Rust does with the declaration, and nothing
   * more: a directory row is `unavailable` with no browser declared, `open dir
   * <name>` resolves `<name>` against the declared entries, and `go to parent`
   * needs a declared `..`. `forced` answers a dispatch regardless, for the
   * cases where the browser moves AFTER Rust judged the utterance.
   */
  function browserVoice(declared: () => VoiceDirectoriesDto | undefined, options: { forced?: boolean; during?: () => Promise<void> } = {}): ResolveVoice {
    return vi.fn(async (utterance: string) => {
      const directories = declared();
      await options.during?.();
      const unavailable = (action: string, hint: string): VoiceResultDto => ({ resolveMs: 21, backend: "stub", outcome: { kind: "unavailable", transcript: utterance, action, hint, sentence: `Not here — ${hint}.` } });
      if (utterance.startsWith("open dir ")) {
        const name = utterance.slice("open dir ".length);
        if (!directories && !options.forced) return unavailable("open_dir", "opening a directory needs the New agent dialog's directory listing; say “new agent” and choose a daemon first");
        const entry = directories?.entries.find((candidate) => candidate.name === name) ?? { name, path: `/home/dev/${name}` };
        return dispatch("open_dir", "openDirectory", `Opening ${entry.name}.`, utterance, [{ name: "dir", kind: "dir_ref", spoken: name, value: entry.path, label: entry.name }]);
      }
      if (utterance === "go to parent") {
        if (!directories?.hasParent && !options.forced) return unavailable("go_to_parent", "going up needs the New agent dialog showing a directory below the top; choose a daemon and open a directory first");
        return dispatch("go_to_parent", "goToParentDirectory", "Going up.", utterance);
      }
      if (utterance === "use this directory") {
        if (!directories && !options.forced) return unavailable("use_this_directory", "choosing a directory needs the New agent dialog's directory listing; say “new agent” and choose a daemon first");
        return dispatch("use_this_directory", "useThisDirectory", "Using this directory.", utterance);
      }
      return dispatch("close", "closeTopmost", "Closed.", utterance);
    });
  }

  /** A runtime over the one-deck fixture that can run the New agent flow and records every declaration. */
  function browsingDeck(voice: VoiceControls, options: { forced?: boolean; during?: () => Promise<void>; startAt?: string; holdStart?: boolean } = {}) {
    const declarations: (VoiceDirectoriesDto | undefined)[] = [];
    const declareVoiceScreen = vi.fn((_screen: string, directories?: VoiceDirectoriesDto) => { declarations.push(directories); });
    const listDirectories = vi.fn(async (_deckId: string, path?: string) => listing(path ?? options.startAt ?? "/home/dev"));
    const runAction = vi.fn((action: { type: string }) => action.type === "start_agent" && options.holdStart
      ? new Promise<DeckActionResult>(() => {})
      : Promise.resolve({ ok: true } as DeckActionResult));
    const deck = runtime(browserVoice(() => declarations.at(-1), options), voice, {
      declareVoiceScreen,
      runAction,
      listDirectories,
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, agents: [], experimental: false, authoringKinds: [] })),
    } as Partial<DeckRuntimeState>);
    return { deck, declarations, listDirectories };
  }

  /** Open the dialog on the one deck, whose preselection chooses it and lists its home. */
  async function openBrowser() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();
    expect(screen.getByTestId("new-agent-directory-list")).toBeInTheDocument();
  }

  const currentPath = () => screen.getByTestId("new-agent-current-path").textContent;

  /**
   * Scenario: open the New agent dialog, then say "open dir billing". The
   * utterance is declared with the browser's children on screen, and the
   * browser goes into `billing` exactly as a click on that row would.
   */
  it("opens a directory named on screen", async () => {
    const voice = microphone([]);
    const { deck, declarations, listDirectories } = browsingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();
    const deckId = listDirectories.mock.calls[0][0];

    voice.deliver("open dir billing");
    await completeUtterance();
    await flush();

    expect(declarations.at(-1)).toEqual({
      deckId,
      path: "/home/dev",
      hasParent: true,
      entries: [{ name: "billing", path: "/home/dev/billing" }, { name: "docs", path: "/home/dev/docs" }],
    });
    expect(listDirectories).toHaveBeenLastCalledWith(deckId, "/home/dev/billing");
    expect(currentPath()).toBe("/home/dev/billing");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening billing.");
  });

  /**
   * Scenario: say "go to parent". The browser goes up to `..` — the same move
   * as `h` or the `..` row — and lands with the cursor on the directory it
   * left.
   */
  it("goes up to the parent", async () => {
    const voice = microphone([]);
    const { deck, listDirectories } = browsingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();

    voice.deliver("go to parent");
    await completeUtterance();
    await flush();

    expect(listDirectories).toHaveBeenLastCalledWith(expect.any(String), "/home");
    expect(currentPath()).toBe("/home");
    expect(screen.getByTestId("new-agent-directory-list").querySelector("[aria-selected='true']")?.getAttribute("data-path")).toBe("/home/dev");
  });

  /**
   * Scenario: say "use this directory". The directory on screen becomes the
   * one the agent starts in and the form unlocks — Space's move — and nothing
   * is started.
   */
  it("uses the directory on screen, which unlocks the form", async () => {
    const voice = microphone([]);
    const { deck } = browsingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();
    expect(screen.getByTestId("new-agent-name")).toBeDisabled();

    voice.deliver("use this directory");
    await completeUtterance();

    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev");
    expect(screen.getByTestId("new-agent-name")).toBeEnabled();
    expect(screen.getByTestId("new-agent-start")).toBeEnabled();
    expect(deck.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: with a filter typed, only the rows the filter leaves on screen
   * are declared — a spoken name means a directory the user can SEE.
   */
  it("declares only the children the filter leaves on screen", async () => {
    const voice = microphone([]);
    const { deck, declarations } = browsingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "doc" } });

    voice.deliver("use this directory");
    await completeUtterance();

    expect(declarations.at(-1)?.entries).toEqual([{ name: "docs", path: "/home/dev/docs" }]);
  });

  /**
   * Scenario: with the dialog closed, nothing is declared, so each directory
   * row is refused with its own hint and nothing is listed.
   */
  it("declares nothing with the dialog closed, so each row is refused", async () => {
    const voice = microphone(["open dir billing"]);
    const { deck, declarations, listDirectories } = browsingDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await completeUtterance();

    expect(declarations).toEqual([undefined]);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Not here — opening a directory needs the New agent dialog's directory listing; say “new agent” and choose a daemon first.");
    expect(listDirectories).not.toHaveBeenCalled();
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
  });

  /**
   * Scenario: a directory dispatch that reaches the overview with no dialog
   * to take it — the dialog closed during the round trip — says so in the
   * dialog's words rather than reporting a move that did not happen.
   */
  it("refuses a directory move that arrives with the dialog closed", async () => {
    const voice = microphone(["use this directory"]);
    const { deck } = browsingDeck(voice, { forced: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(NO_DIRECTORY_BROWSER);
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
  });

  /**
   * Scenario: the user clicks into another directory while "open dir docs" is
   * being resolved. The answer was about the listing they left, so the browser
   * is left where the click put it and the report says why.
   */
  it("refuses a move judged against a listing the browser has since left", async () => {
    const voice = microphone([]);
    /* The resolve waits on a gate, so the click below — and its listing
       landing — happen during the round trip and commit on their own, as they
       do in the app, where the answer takes hundreds of milliseconds. */
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const { deck, listDirectories } = browsingDeck(voice, { during: () => gate });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();

    voice.deliver("open dir docs");
    await completeUtterance();
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev/billing']")!);
    await flush();
    expect(currentPath()).toBe("/home/dev/billing");

    release();
    await flush();
    await flush();

    expect(currentPath()).toBe("/home/dev/billing");
    expect(listDirectories).not.toHaveBeenCalledWith(expect.any(String), "/home/dev/docs");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIRECTORY_MOVED_ON);
  });

  /**
   * Scenario: with the filter at "doc", say "open dir docs", and change the
   * filter to "bill" while it is being resolved. The deck and the listing are
   * unchanged, but `docs` is no longer on screen, so nothing is opened and the
   * report says why.
   */
  it("refuses a directory the filter hid during the round trip", async () => {
    const voice = microphone([]);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const { deck, declarations, listDirectories } = browsingDeck(voice, { during: () => gate });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "doc" } });

    voice.deliver("open dir docs");
    await completeUtterance();
    expect(declarations.at(-1)?.entries).toEqual([{ name: "docs", path: "/home/dev/docs" }]);
    fireEvent.change(screen.getByTestId("new-agent-filter"), { target: { value: "bill" } });
    await flush();

    release();
    await flush();
    await flush();

    expect(currentPath()).toBe("/home/dev");
    expect(listDirectories).not.toHaveBeenCalledWith(expect.any(String), "/home/dev/docs");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIRECTORY_NOT_LISTED);
  });

  /**
   * Scenario: at the filesystem root there is no `..`. The declaration says
   * so, "go to parent" is refused with its hint, and a dispatch that arrives
   * anyway is refused by the dialog.
   */
  it("refuses to go up from a root", async () => {
    const voice = microphone([]);
    const { deck, declarations, listDirectories } = browsingDeck(voice, { startAt: "/" });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();

    voice.deliver("go to parent");
    await completeUtterance();
    expect(declarations.at(-1)?.hasParent).toBe(false);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Not here — going up needs the New agent dialog showing a directory below the top; choose a daemon and open a directory first.");
    expect(listDirectories).toHaveBeenCalledTimes(1);
    expect(currentPath()).toBe("/");
  });

  /**
   * Scenario: at a root, a go-up that reaches the dialog anyway is refused in
   * the dialog's own sentence and lists nothing.
   */
  it("refuses a forced go-up at a root in the dialog's own words", async () => {
    const voice = microphone([]);
    const { deck, listDirectories } = browsingDeck(voice, { startAt: "/", forced: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();

    voice.deliver("go to parent");
    await completeUtterance();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(NO_PARENT_DIRECTORY);
    expect(listDirectories).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: with a start in flight every browser control is disabled, so
   * nothing is declared and a directory move that arrives is refused.
   */
  it("declares nothing and moves nothing while a start is in flight", async () => {
    const voice = microphone([]);
    const { deck, declarations } = browsingDeck(voice, { forced: true, holdStart: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openBrowser();
    fireEvent.keyDown(screen.getByTestId("new-agent-directory-list"), { key: " " });
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-start"));
    await flush();
    expect(screen.getByTestId("new-agent-starting")).toBeInTheDocument();

    voice.deliver("open dir billing");
    await completeUtterance();
    expect(declarations.at(-1)).toBeUndefined();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(NO_DIRECTORY_BROWSER);
    expect(currentPath()).toBe("/home/dev");
  });

  /**
   * Scenario: off the overview the dialog is not mounted, nothing is declared,
   * and a directory dispatch finds no one to serve it.
   */
  it("is not served off the overview", async () => {
    const voice = microphone(["use this directory"]);
    const { deck, declarations } = browsingDeck(voice, { forced: true });
    render(<DeckShell runtime={deck} />);
    await turnVoiceOn();
    await completeUtterance();

    expect(declarations).toEqual([undefined]);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(NOTHING_DISPATCHED);
  });
});

describe("the rest of the New agent form, by voice (PRD #1223)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const HOME = {
    kind: "listing" as const,
    path: "/home/dev",
    displayPath: "/home/dev",
    parent: "/home",
    entries: [{ path: "/home/dev/billing", displayName: "billing", isProject: true }],
    truncated: false,
  };
  const BILLING = { ...HOME, path: "/home/dev/billing", displayPath: "/home/dev/billing", parent: "/home/dev", entries: [] };
  const AGENTS = [
    { id: "claude", displayName: "Claude Code", defaultCommand: "claude --model haiku" },
    { id: "opencode", displayName: "OpenCode", defaultCommand: "opencode" },
  ];

  /**
   * A resolver that does what Rust does with the form declaration: a fill row
   * is `unavailable` with no live form declared, and a spoken mode or agent
   * type resolves against the chips and entries AS DECLARED — so a chip the
   * form does not offer is `param_unresolved`. `forced` dispatches regardless,
   * for a form that moves after Rust judged the utterance.
   */
  function formVoice(declared: () => VoiceNewAgentDto | undefined, options: { forced?: boolean; during?: () => Promise<void> } = {}): ResolveVoice {
    return vi.fn(async (utterance: string) => {
      const form = declared()?.form;
      await options.during?.();
      const unavailable = (action: string, hint: string): VoiceResultDto => ({ resolveMs: 21, backend: "stub", outcome: { kind: "unavailable", transcript: utterance, action, hint, sentence: `Not here — ${hint}.` } });
      const unresolved = (action: string, param: string, spoken: string, sentence: string): VoiceResultDto => ({ resolveMs: 21, backend: "stub", outcome: { kind: "param_unresolved", transcript: utterance, action, param, spoken, sentence } });
      if (utterance.startsWith("mode ")) {
        const spoken = utterance.slice("mode ".length);
        if (!form && !options.forced) return unavailable("choose_mode", "choosing a mode needs a daemon and a directory chosen in the New agent dialog; choose those first");
        const chip = form?.modes.find((candidate) => candidate.label.toLowerCase() === spoken) ?? (options.forced ? { id: spoken, label: spoken } : undefined);
        if (!chip) return unresolved("choose_mode", "mode", spoken, `Heard: “${utterance}” — no mode the New agent form offers matches “${spoken}”.`);
        return dispatch("choose_mode", "chooseNewAgentMode", `Mode: ${chip.label}.`, utterance, [{ name: "mode", kind: "mode_ref", spoken, value: chip.id, label: chip.label }]);
      }
      if (utterance.startsWith("use ")) {
        const spoken = utterance.slice("use ".length);
        if (!form && !options.forced) return unavailable("choose_agent_type", "choosing an agent needs a daemon and a directory chosen in the New agent dialog; choose those first");
        const entry = form?.agentTypes.find((candidate) => candidate.id === spoken || candidate.label.toLowerCase() === spoken);
        if (!entry) return unresolved("choose_agent_type", "agent_type", spoken, `Heard: “${utterance}” — no agent this daemon offers matches “${spoken}”.`);
        return dispatch("choose_agent_type", "chooseNewAgentType", `Command set to ${entry.label}'s default command.`, utterance, [{ name: "agent_type", kind: "agent_type_ref", spoken, value: entry.id, label: entry.label }]);
      }
      if (utterance.startsWith("call it ")) {
        if (!form && !options.forced) return unavailable("name_new_agent", "naming the new agent needs a daemon and a directory chosen in the New agent dialog; choose those first");
        const rest = utterance.slice("call it ".length);
        return dispatch("name_new_agent", "nameNewAgent", "Name set.", utterance, [{ name: "prefix", kind: "spoken_prefix", spoken: "call it", value: rest, label: rest }]);
      }
      /* No row fills Command: the model's escape, which renders "no matching action". */
      return { resolveMs: 21, backend: "stub", outcome: { kind: "no_match", transcript: utterance, sentence: `Heard: “${utterance}” — no matching action.` } };
    });
  }

  /** A deck whose options, listing and orchestrations the flow can load, recording each declaration. */
  function formDeck(voice: VoiceControls, options: { forced?: boolean; during?: () => Promise<void>; experimental?: boolean } = {}) {
    const declarations: (VoiceNewAgentDto | undefined)[] = [];
    const declareVoiceScreen = vi.fn((_screen: string, _directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto) => { declarations.push(newAgent); });
    const deck = runtime(formVoice(() => declarations.at(-1), options), voice, {
      declareVoiceScreen,
      runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
      listDirectories: vi.fn(async (_deckId: string, path?: string) => (path === "/home/dev/billing" ? BILLING : HOME)),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, defaultCommand: "bash", agents: AGENTS, experimental: options.experimental ?? false, authoringKinds: ["schedule", "schedule-issues", "dispatcher"] })),
      newAgentOrchestrations: vi.fn(async (_deckId: string, path: string) => ({
        kind: "project" as const,
        path,
        displayPath: path,
        displayName: "billing",
        orchestrations: [{ name: "review", displayName: "review", default: true, roles: [{ name: "lead", displayName: "lead", start: true }, { name: "critic", displayName: "critic", start: false }] }],
      })),
    } as Partial<DeckRuntimeState>);
    return { deck, declarations };
  }

  /** Open the dialog and choose `billing`, a project, so the form is live with its orchestration chip. */
  async function openForm() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev/billing']")!);
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await flush();
    await flush();
    expect(screen.getByTestId("new-agent-name")).toBeEnabled();
  }

  const pressedMode = () => screen.getByTestId("new-agent-modes").querySelector("[aria-pressed='true']")?.getAttribute("data-mode");

  /**
   * Scenario: with the form live, say "mode dispatcher". The declaration
   * carries the chips as offered, and the Dispatcher chip is pressed exactly as
   * a click presses it; nothing is started.
   */
  it("chooses a Mode chip the form offers", async () => {
    const voice = microphone([]);
    const { deck, declarations } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode dispatcher");
    await completeUtterance();

    expect(declarations.at(-1)?.form?.modes.map((mode) => mode.label)).toEqual(["No mode", "Orch: review", "schedule", "dispatcher"]);
    expect(pressedMode()).toBe("dispatcher");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Mode: dispatcher.");
    expect(deck.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: say "mode orch: review". The orchestration chip is chosen, and —
   * as a click does — Command is hidden and the Name follows the TUI's
   * orchestration suggestion.
   */
  it("chooses an orchestration chip, with the click's own side effects", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode orch: review");
    await completeUtterance();

    expect(pressedMode()).toBe("orch:review");
    expect(screen.queryByTestId("new-agent-command")).toBeNull();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("billing-orchestrator-1");
  });

  /**
   * Scenario: on a daemon whose experimental flag is off, `schedule: issues` is
   * not a chip at all. It is absent from the declaration, so "mode schedule:
   * issues" is refused as not offered and the Mode is left alone.
   */
  it("refuses a chip the form does not offer", async () => {
    const voice = microphone([]);
    const { deck, declarations } = formDeck(voice, { experimental: false });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode schedule: issues");
    await completeUtterance();

    expect(declarations.at(-1)?.form?.modes.some((mode) => mode.id === "schedule-issues")).toBe(false);
    expect(declarations.at(-1)?.form?.withheldModes).toEqual([{ id: "schedule-issues", label: "schedule: issues" }]);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("no mode the New agent form offers matches “schedule: issues”");
    expect(pressedMode()).toBe("none");
  });

  /** Scenario: with the flag ON, the same chip IS declared, and choosing it works. */
  it("offers schedule: issues when the daemon's flag is on", async () => {
    const voice = microphone([]);
    const { deck, declarations } = formDeck(voice, { experimental: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode schedule: issues");
    await completeUtterance();

    expect(pressedMode()).toBe("schedule-issues");
    expect(declarations.at(-1)?.form?.withheldModes).toEqual([]);
  });

  /**
   * Scenario: a mode dispatch that arrives for a chip the dialog no longer
   * shows is refused in the dialog's words, not applied.
   */
  it("refuses a forced chip the dialog does not show", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice, { forced: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode workspace");
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(MODE_NOT_OFFERED);
    expect(pressedMode()).toBe("none");
  });

  /**
   * Scenario: say "use claude". There is no Agent picker any more (PRD #1223),
   * so Command is overwritten with Claude Code's default command and the report
   * says so. The declaration is the daemon's registry, with no `auto`.
   */
  it("chooses an agent type by filling Command with its default command", async () => {
    const voice = microphone([]);
    const { deck, declarations } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();
    expect(screen.getByTestId("new-agent-command")).toHaveValue("bash");

    voice.deliver("use claude");
    await completeUtterance();

    expect(declarations.at(-1)?.form?.agentTypes).toEqual([{ id: "claude", label: "Claude Code" }, { id: "opencode", label: "OpenCode" }]);
    expect(screen.queryByTestId("new-agent-agent")).toBeNull();
    expect(screen.getByTestId("new-agent-command")).toHaveValue("claude --model haiku");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Command set to Claude Code's default command.");
  });

  /**
   * Scenario: choose the review orchestration, then say "use claude". Its
   * roles run their own commands and Command is hidden, so nothing is filled
   * behind the user's back: the dialog refuses, and choosing No mode again
   * shows Command as it was.
   */
  it("refuses to fill Command while an orchestration is selected", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode orch: review");
    await completeUtterance();
    voice.deliver("use claude");
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(COMMAND_HIDDEN_BY_ORCHESTRATION);
    fireEvent.click(screen.getByTestId("new-agent-mode-none"));
    expect(screen.getByTestId("new-agent-command")).toHaveValue("bash");
  });

  /** Scenario: an agent the daemon does not offer is refused, and Command is left alone. */
  it("refuses an agent type the daemon does not offer", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("use codex");
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent("no agent this daemon offers matches “codex”");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("bash");
  });

  /**
   * Scenario: say "call it billing worker." The Name field takes the words
   * after the boundary with the transcriber's full stop trimmed, and counts as
   * an edit — a later orchestration chip no longer replaces it.
   */
  it("names the agent from the words after the boundary", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("call it billing worker.");
    await completeUtterance();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("billing worker");
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Name set.");

    fireEvent.click(screen.getByTestId("new-agent-mode-orch:review"));
    await flush();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("billing worker");
  });

  it("trims only the sentence punctuation around a spoken name", () => {
    expect(spokenName(" billing worker. ")).toBe("billing worker");
    expect(spokenName("api-v2!?")).toBe("api-v2");
    expect(spokenName("docs.site")).toBe("docs.site");
    expect(spokenName(" . ")).toBe("");
    expect(spokenName(undefined)).toBe("");
  });

  /**
   * Scenario: Command has no voice row. Saying something about the command
   * reaches no fill member, and the Command field keeps what it had.
   */
  it("leaves Command manual", async () => {
    const voice = microphone([]);
    const { deck } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("set the command to rm -rf");
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent("no matching action");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("bash");
  });

  /**
   * Scenario: before a directory is chosen the form is not live, so no form is
   * declared and each fill row is refused with its hint.
   */
  it("declares no form until a directory is chosen, so each fill row is refused", async () => {
    const voice = microphone([]);
    const { deck, declarations } = formDeck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();

    voice.deliver("mode dispatcher");
    await completeUtterance();

    expect(declarations.at(-1)).toEqual({ form: undefined });
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Not here — choosing a mode needs a daemon and a directory chosen in the New agent dialog; choose those first.");
  });

  /** Scenario: with the dialog closed nothing is declared, and a forced fill finds no form. */
  it("refuses a fill that arrives with the dialog closed", async () => {
    const voice = microphone(["mode dispatcher"]);
    const { deck, declarations } = formDeck(voice, { forced: true });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await completeUtterance();

    expect(declarations).toEqual([undefined]);
    expect(screen.getByTestId("voice-report")).toHaveTextContent(NO_NEW_AGENT_FORM);
  });

  /**
   * Scenario: the user chooses a different directory while "mode dispatcher"
   * is being resolved. The answer was about the form they left, so the Mode is
   * not changed and the report says why.
   */
  it("refuses a fill judged against a form that has since moved on", async () => {
    const voice = microphone([]);
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const { deck } = formDeck(voice, { during: () => gate });
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openForm();

    voice.deliver("mode dispatcher");
    await completeUtterance();
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev']")!);
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await flush();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev");

    release();
    await flush();
    await flush();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(FORM_MOVED_ON);
    expect(pressedMode()).toBe("none");
  });
});

describe("PRD #802 D5 — a spoken stop opens a confirmation; a spoken start starts (PRD #1223, D5 revisited 2026-09-23)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const HOME = {
    kind: "listing" as const,
    path: "/home/dev",
    displayPath: "/home/dev",
    parent: "/home",
    entries: [{ path: "/home/dev/billing", displayName: "billing", isProject: true }],
    truncated: false,
  };
  const BILLING = { ...HOME, path: "/home/dev/billing", displayPath: "/home/dev/billing", parent: "/home/dev", entries: [] };

  /**
   * What Rust answers for the start and the two D5 stops, and nothing else:
   * `start it` starts when the dialog is OPEN (any form) and opens it when it
   * is closed, `stop <id>` resolves an agent id on the
   * selected deck, and `close <member>` names an orchestration card by one
   * member's id. Each is only a DISPATCH — the tests below are about what the
   * app does with it.
   */
  function d5Voice(declared: () => VoiceNewAgentDto | undefined): ResolveVoice {
    return vi.fn(async (utterance: string) => {
      // "Activate orchestration" is the Start button's own label with an
      // orchestration chosen — the label rule (PRD #1223).
      if (utterance === "start it" || utterance === "start the new agent" || utterance === "Activate orchestration") {
        // With the dialog closed the callable row that answers "start" is
        // `open_new_agent` (D3); with it open, `start_new_agent` starts.
        if (!declared()) return dispatch("open_new_agent", "openNewAgent", "Opening the New agent dialog.", utterance);
        return dispatch("start_new_agent", "startNewAgent", "Starting the agent.", utterance);
      }
      if (utterance.startsWith("stop ")) {
        const id = utterance.slice("stop ".length);
        return dispatch("stop_agent", "confirmStopAgent", `Confirm stopping ${id} — nothing has been stopped yet.`, utterance, [{ name: "agent", kind: "agent_ref", spoken: id, value: id, label: id }]);
      }
      if (utterance.startsWith("close ")) {
        const member = utterance.slice("close ".length);
        return dispatch("close_orchestration", "confirmCloseOrchestration", "Confirm closing review — nothing has been stopped yet.", utterance, [{ name: "orchestration", kind: "orchestration_ref", spoken: "review", value: member, label: "review" }]);
      }
      if (utterance === "mode dispatcher") {
        return dispatch("choose_mode", "chooseNewAgentMode", "Mode: dispatcher.", utterance, [{ name: "mode", kind: "mode_ref", spoken: "dispatcher", value: "dispatcher", label: "dispatcher" }]);
      }
      return { resolveMs: 21, backend: "stub", outcome: { kind: "no_match", transcript: utterance, sentence: `Heard: “${utterance}” — no matching action.` } } as VoiceResultDto;
    });
  }

  /** The fixture deck with two of its agents made roles of one `review` orchestration. */
  function d5Deck(voice: VoiceControls) {
    const declarations: (VoiceNewAgentDto | undefined)[] = [];
    const snapshot = createFixtureSnapshot("connected");
    snapshot.agents = snapshot.agents.map((agent, index) => (index < 2
      ? { ...agent, tab: { kind: "orchestration" as const, orchestrationId: "o-review", name: "review", displayTitle: "review", roleName: index === 0 ? "lead" : "critic", roleIndex: index, isStartRole: index === 0 } }
      : agent));
    const runAction = vi.fn(async (action: { type: string }) => (action.type === "start_agent" ? { ok: true, agentId: "new-1" } : { ok: true }) as DeckActionResult);
    const deck = runtime(d5Voice(() => declarations.at(-1)), voice, {
      snapshot,
      fleet: [snapshot],
      declareVoiceScreen: vi.fn((_screen: string, _directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto) => { declarations.push(newAgent); }),
      runAction,
      listDirectories: vi.fn(async (_deckId: string, path?: string) => (path === "/home/dev/billing" ? BILLING : HOME)),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, defaultCommand: "claude --model haiku", agents: [{ id: "claude", displayName: "Claude Code", defaultCommand: "claude" }], experimental: false, authoringKinds: ["dispatcher"] })),
      newAgentOrchestrations: vi.fn(async (_deckId: string, path: string) => ({
        kind: "project" as const,
        path,
        displayPath: path,
        displayName: "billing",
        orchestrations: [{ name: "audit", displayName: "audit", default: true, roles: [{ name: "lead", displayName: "lead", start: true }, { name: "checker", displayName: "checker", start: false }] }],
      })),
    } as Partial<DeckRuntimeState>);
    return { deck, runAction, snapshot, declarations };
  }

  async function openDialog() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();
  }

  async function chooseBilling() {
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev/billing']")!);
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await flush();
    await flush();
  }

  const confirmation = () => screen.queryByRole("alertdialog");

  /**
   * Scenario: with the form filled, say "start it". The agent starts at once
   * with exactly what the form shows, and no confirmation opens — the user's
   * words were "it only introduced friction by me having to give the same
   * instruction twice" (PRD #802 D5, revisited for the start only).
   */
  it("starts exactly the form at once, with no confirmation", async () => {
    const voice = microphone([]);
    const { deck, runAction, snapshot } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "billing-worker" } });

    voice.deliver("start it");
    await completeUtterance();
    await flush();

    expect(confirmation()).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Starting the agent.");
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({
      type: "start_agent",
      deckId: snapshot.connection.deckId,
      cwd: "/home/dev/billing",
      command: "claude --model haiku",
      displayName: "billing-worker",
    });
  });

  /**
   * Scenario: say "start it" before a directory is chosen. Nothing starts and
   * no confirmation opens; the voice surface says what is missing.
   */
  it("refuses an incomplete form with the reason", async () => {
    const voice = microphone([]);
    const { deck, runAction, declarations } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();

    voice.deliver("start it");
    await completeUtterance();

    expect(declarations.at(-1)).toEqual({ form: undefined });
    expect(confirmation()).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(START_NEEDS_DIRECTORY);
    expect(runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: with the dialog closed, "start the new agent" opens it (D3) —
   * Rust answers the callable `open_new_agent` row — and starts nothing.
   */
  it("opens the dialog when a start is asked for with it closed", async () => {
    const voice = microphone(["start the new agent"]);
    const { deck, runAction } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await completeUtterance();
    await flush();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(confirmation()).toBeNull();
    expect(runAction).not.toHaveBeenCalled();
    // A start dispatch that arrives anyway finds no dialog to start from.
    expect(NO_NEW_AGENT_DIALOG).toContain("nothing was started");
  });

  /**
   * Scenario: a second "start it" while the first start is still in flight
   * starts nothing more, and says why.
   */
  it("refuses a second start while one is under way", async () => {
    const voice = microphone([]);
    const { deck, runAction } = d5Deck(voice);
    runAction.mockImplementation(() => new Promise(() => {}));
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();

    voice.deliver("start it");
    await completeUtterance();
    voice.deliver("start it");
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(START_IN_FLIGHT);
    expect(confirmation()).toBeNull();
    expect(runAction).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: with an orchestration chosen, "start it" launches it at once,
   * with no confirmation.
   */
  it("starts an orchestration at once", async () => {
    const voice = microphone([]);
    const { deck, runAction } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();
    fireEvent.click(screen.getByTestId("new-agent-mode-orch:audit"));
    await flush();

    voice.deliver("start it");
    await completeUtterance();
    await flush();

    expect(confirmation()).toBeNull();
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction.mock.calls[0][0]).toMatchObject({ type: "start_orchestration", orchestration: "audit", path: "/home/dev/billing" });
  });

  /**
   * Scenario: the user's own report. With the `audit` orchestration chosen in
   * Mode the Start button reads "Activate orchestration"; the user reads it aloud
   * and the orchestration launches — the button's words work as a command.
   */
  it("starts the chosen orchestration when the Start button's label is said", async () => {
    const voice = microphone([]);
    const { deck, runAction } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();
    fireEvent.click(screen.getByTestId("new-agent-mode-orch:audit"));
    await flush();
    expect(screen.getByTestId("new-agent-start")).toHaveTextContent("Activate orchestration");

    voice.deliver("Activate orchestration");
    await completeUtterance();
    await flush();

    expect(confirmation()).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Starting the agent.");
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction.mock.calls[0][0]).toMatchObject({ type: "start_orchestration", orchestration: "audit", path: "/home/dev/billing" });
  });

  /** Scenario: the MANUAL Start button is unchanged — it starts at once, with no confirmation. */
  it("leaves the manual Start button as it was", async () => {
    const voice = microphone([]);
    const { deck, runAction } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();

    await act(async () => {
      fireEvent.click(screen.getByTestId("new-agent-start"));
      await Promise.resolve();
    });
    await flush();

    expect(confirmation()).toBeNull();
    expect(runAction).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: say "stop planner". The overview opens the SAME confirmation its
   * row's Stop button opens, and nothing is stopped until it is confirmed.
   */
  it("opens the existing stop confirmation and stops nothing until it is confirmed", async () => {
    const voice = microphone([]);
    const { deck, runAction, snapshot } = d5Deck(voice);
    const target = snapshot.agents[2];
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();

    voice.deliver(`stop ${target.id}`);
    await completeUtterance();

    const dialog = confirmation();
    expect(dialog).not.toBeNull();
    expect(dialog).toHaveTextContent("This sends a stop request to");
    expect(screen.getByRole("button", { name: "Close agent" })).toBeInTheDocument();
    expect(runAction).not.toHaveBeenCalled();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Close agent" }));
      await Promise.resolve();
    });
    await flush();
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: snapshot.connection.deckId, agentId: target.id });
  });

  /**
   * Scenario: say "close" naming a role of the `review` orchestration. The
   * card's own confirmation opens, naming every role it will stop, and nothing
   * stops until it is confirmed.
   */
  it("opens the existing orchestration confirmation, naming its roles", async () => {
    const voice = microphone([]);
    const { deck, runAction, snapshot } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();

    voice.deliver(`close ${snapshot.agents[1].id}`);
    await completeUtterance();

    const dialog = confirmation();
    expect(dialog).toHaveTextContent("Close review?");
    expect(dialog).toHaveTextContent("all 2 of its roles: lead, critic");
    expect(runAction).not.toHaveBeenCalled();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Stop all 2 roles" }));
      await Promise.resolve();
    });
    await flush();
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction.mock.calls[0][0]).toMatchObject({ type: "stop_orchestration", deckId: snapshot.connection.deckId });
  });

  /** Scenario: an agent that has left the overview is refused, and no confirmation opens. */
  it("refuses a stop for an agent no longer on the overview", async () => {
    const voice = microphone(["stop ghost"]);
    const { deck, runAction } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(STOP_TARGET_GONE);
    expect(confirmation()).toBeNull();
    expect(runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: a second spoken stop while one confirmation is open would
   * replace it under the user's pointer; it is refused, and the first stays.
   */
  it("never replaces a confirmation that is already open", async () => {
    const voice = microphone([]);
    const { deck, snapshot } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();

    voice.deliver(`stop ${snapshot.agents[2].id}`);
    await completeUtterance();
    const first = confirmation()?.textContent;
    voice.deliver(`stop ${snapshot.agents[3].id}`);
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(CONFIRMATION_ALREADY_OPEN);
    expect(confirmation()?.textContent).toBe(first);
  });

  /** Scenario: with the New agent dialog open, a spoken stop is refused rather than hidden behind it. */
  it("refuses a stop while the New agent dialog is open", async () => {
    const voice = microphone([]);
    const { deck, runAction, snapshot } = d5Deck(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();

    voice.deliver(`stop ${snapshot.agents[2].id}`);
    await completeUtterance();

    expect(screen.getByTestId("voice-report")).toHaveTextContent(STOP_BEHIND_NEW_AGENT);
    expect(confirmation()).toBeNull();
    expect(runAction).not.toHaveBeenCalled();
  });
});

describe("a pending answer and a New agent dialog that changed under it (PRD #1223 audit I1)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const HOME = {
    kind: "listing" as const,
    path: "/home/dev",
    displayPath: "/home/dev",
    parent: "/home",
    entries: [{ path: "/home/dev/billing", displayName: "billing", isProject: false }],
    truncated: false,
  };
  const BILLING = { ...HOME, path: "/home/dev/billing", displayPath: "/home/dev/billing", parent: "/home/dev", entries: [] };

  const ANSWERS: Record<string, VoiceResultDto> = {
    close: dispatch("close", "closeTopmost", "Closed.", "done"),
    open_deck: dispatch("open_deck", "openDeck", "Back to the daemon.", "back"),
  };

  /**
   * A resolver held open until the test releases it, answering with a steered
   * `close` or `open_deck` — the answers the ordinary token list lets through
   * and the dialog's own grounding would have refused.
   */
  function heldDeck(voice: VoiceControls, answer: VoiceResultDto) {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const resolveVoice: ResolveVoice = vi.fn(async () => {
      await gate;
      return answer;
    });
    const deck = runtime(resolveVoice, voice, {
      runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
      listDirectories: vi.fn(async (_deckId: string, path?: string) => (path === "/home/dev/billing" ? BILLING : HOME)),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, defaultCommand: "bash", agents: [], experimental: false, authoringKinds: [] })),
    } as Partial<DeckRuntimeState>);
    return { deck, release };
  }

  async function openDialog() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();
  }

  async function chooseBilling() {
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev/billing']")!);
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await flush();
    await flush();
  }

  /**
   * Scenario: with no dialog open, say "done" (or "back"). While it is being
   * worked out, open the New agent dialog, choose a directory and type a Name.
   * The answer arrives as a `close` (or `open_deck`): it was judged with no
   * dialog declared, so it runs nothing — the dialog and its draft stay, and
   * the report says why.
   */
  it.each(Object.keys(ANSWERS))("refuses the %s answer judged before the dialog opened", async (kind) => {
    const voice = microphone([]);
    const { deck, release } = heldDeck(voice, ANSWERS[kind]);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();

    voice.deliver(kind === "close" ? "done" : "back");
    await completeUtterance();
    expect(deck.resolveVoice).toHaveBeenCalledTimes(1);
    await openDialog();
    await chooseBilling();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "draft" } });

    release();
    await flush();
    await flush();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("draft");
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/billing");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIALOG_MOVED_ON);
  });

  /**
   * Scenario: with the dialog open but no directory chosen yet, say "done".
   * While it is being worked out, choose a directory, which makes the form
   * live. The `close` that arrives was judged against the dialog without a
   * form, so it runs nothing and the form stays.
   */
  it("refuses a close answer judged before the form became live", async () => {
    const voice = microphone([]);
    const { deck, release } = heldDeck(voice, ANSWERS.close);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();

    voice.deliver("done");
    await completeUtterance();
    await chooseBilling();
    expect(screen.getByTestId("new-agent-name")).toBeEnabled();

    release();
    await flush();
    await flush();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/billing");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIALOG_MOVED_ON);
  });

  /**
   * Scenario: with the dialog open and its form live, say "done". While it is
   * being worked out, close the dialog and open it again — a NEW dialog, which
   * restores the closed one's draft (#1247) and is then edited. The `close`
   * that arrives was grounded against the first one, so it runs nothing: the
   * replacement and its draft stay. Both declarations name a live form, so
   * nothing but the dialog's identity separates them (Qodo on PR #1235).
   */
  it("refuses a close answer grounded against a dialog the user has since reopened", async () => {
    const voice = microphone([]);
    const { deck, release } = heldDeck(voice, ANSWERS.close);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();

    voice.deliver("done");
    await completeUtterance();

    await act(async () => { fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" }); });
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    await openDialog();
    await flush();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/billing");
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "draft" } });

    release();
    await flush();
    await flush();

    expect(screen.getByTestId("new-agent-dialog")).toBeInTheDocument();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("draft");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIALOG_MOVED_ON);
  });

  /**
   * Scenario: the control — with the dialog unchanged across the round trip,
   * the same held `close` does close it. The refusal above is about the
   * declaration moving, not about a slow answer.
   */
  it("runs the answer when the dialog did not change while it was pending", async () => {
    const voice = microphone([]);
    const { deck, release } = heldDeck(voice, ANSWERS.close);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    await chooseBilling();

    voice.deliver("done");
    await completeUtterance();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "draft" } });

    release();
    await flush();
    await flush();

    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
  });
});

describe("the New agent deck field and Discard, by voice (issues 1263 and 1247)", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const HOME = {
    kind: "listing" as const,
    path: "/home/dev",
    displayPath: "/home/dev",
    parent: "/home",
    entries: [{ path: "/home/dev/billing", displayName: "billing", isProject: false }],
    truncated: false,
  };
  const BILLING = { ...HOME, path: "/home/dev/billing", displayPath: "/home/dev/billing", parent: "/home/dev", entries: [] };

  /* What Rust resolves each deck to against the observed fleet — the wire
     `deckId` and the label the overview shows. */
  const DECKS: Record<string, { value: string; label: string }> = {
    "deck build box": { value: FIXTURE_REMOTE_DAEMON_ID, label: "dev@build-box" },
    "deck local": { value: FIXTURE_DAEMON_ID, label: "Local deck" },
    "deck runner": { value: FIXTURE_UNREACHABLE_DAEMON_ID, label: "ci@runner-7" },
    "deck gone": { value: "deck-that-left", label: "gone@nowhere" },
  };

  function answer(utterance: string): VoiceResultDto {
    const named = DECKS[utterance];
    if (named) return dispatch("choose_deck", "chooseNewAgentDeck", `Deck: ${named.label}.`, utterance, [{ name: "deck", kind: "deck_ref", spoken: utterance.slice("deck ".length), value: named.value, label: named.label }]);
    if (utterance === "discard") return dispatch("discard_new_agent", "discardNewAgent", "Discarded the New agent form.", utterance);
    if (utterance === "close") return dispatch("close", "closeTopmost", "Closed.", utterance);
    if (utterance === "mode dispatcher") return dispatch("choose_mode", "chooseNewAgentMode", "Mode: dispatcher.", utterance, [{ name: "mode", kind: "mode_ref", spoken: "dispatcher", value: "dispatcher", label: "dispatcher" }]);
    return { resolveMs: 21, backend: "stub", outcome: { kind: "no_match", transcript: utterance, sentence: `Heard: “${utterance}” — no matching action.` } };
  }

  /**
   * The four-deck fixture fleet — two decks can take a spawn, so the dialog
   * opens with none chosen — whose resolver answers as Rust would, and can be
   * HELD open for a test that changes the dialog during the round trip.
   */
  function deckFieldRuntime(voice: VoiceControls) {
    const fleet = createFixtureFleet("fleet");
    let gate: Promise<void> | undefined;
    let release: () => void = () => undefined;
    const hold = () => { gate = new Promise<void>((resolve) => { release = resolve; }); };
    const resolveVoice: ResolveVoice = vi.fn(async (utterance: string) => {
      if (gate) await gate;
      return answer(utterance);
    });
    const deck = runtime(resolveVoice, voice, {
      snapshot: fleet[0],
      fleet,
      runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
      listDirectories: vi.fn(async (_deckId: string, path?: string) => (path === "/home/dev/billing" ? BILLING : HOME)),
      newAgentOptions: vi.fn(async () => ({ kind: "deck" as const, defaultCommand: "bash", agents: [], experimental: false, authoringKinds: ["dispatcher"] })),
    } as Partial<DeckRuntimeState>);
    return { deck, hold, release: () => release() };
  }

  const chosenDeck = () => screen.getByTestId("new-agent-deck-list").querySelector("[data-chosen='true']")?.getAttribute("data-deck-id");
  const pressedMode = () => screen.getByTestId("new-agent-modes").querySelector("[aria-pressed='true']")?.getAttribute("data-mode");

  async function openDialog() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    await flush();
    await flush();
  }

  async function chooseBilling() {
    fireEvent.click(screen.getByTestId("new-agent-directory-list").querySelector("[data-path='/home/dev/billing']")!);
    await flush();
    fireEvent.click(screen.getByTestId("new-agent-use-directory"));
    await flush();
    await flush();
  }

  /**
   * Scenario: open New agent from the top bar — two decks can take a spawn,
   * so none is chosen — and say "deck build box". That deck is chosen as a
   * click chooses it: it is asked for its options and its home, and the report
   * names it. Then choose a directory, type a Name, and say "deck local": the
   * directory goes, the typed Name stays, and the local deck is asked in turn.
   */
  it("chooses the deck in the open dialog through the click's own path", async () => {
    const voice = microphone([]);
    const { deck } = deckFieldRuntime(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    expect(chosenDeck()).toBeUndefined();

    voice.deliver("deck build box");
    await completeUtterance();
    await flush();

    expect(chosenDeck()).toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(deck.newAgentOptions).toHaveBeenLastCalledWith(FIXTURE_REMOTE_DAEMON_ID);
    expect(deck.listDirectories).toHaveBeenLastCalledWith(FIXTURE_REMOTE_DAEMON_ID, undefined);
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Deck: dev@build-box.");

    await chooseBilling();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });
    voice.deliver("deck local");
    await completeUtterance();
    await flush();

    expect(chosenDeck()).toBe(FIXTURE_DAEMON_ID);
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
    expect(deck.newAgentOptions).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID);
    expect(deck.runAction).not.toHaveBeenCalled();
  });

  /**
   * Scenario: a deck resolved against the fleet reaches a dialog whose field
   * shows it disabled, or no longer lists it at all. Each is refused in the
   * dialog's words and nothing is chosen.
   */
  it.each([
    ["deck runner", DECK_CANNOT_TAKE_AGENT],
    ["deck gone", DECK_NOT_LISTED],
  ])("refuses %s rather than choosing it", async (utterance, refusal) => {
    const voice = microphone([]);
    const { deck } = deckFieldRuntime(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();

    voice.deliver(utterance);
    await completeUtterance();

    expect(chosenDeck()).toBeUndefined();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(refusal);
    expect(deck.newAgentOptions).not.toHaveBeenCalled();
  });

  /**
   * Scenario: with the local deck chosen and no directory yet, say "deck build
   * box"; while it is being worked out, choose a directory by hand. The answer
   * was judged against a dialog with no live form, so it runs nothing: the
   * directory the user just chose is not thrown away by a deck change they
   * asked for before choosing it.
   */
  it("refuses a deck answer judged before a directory was chosen", async () => {
    const voice = microphone([]);
    const { deck, hold, release } = deckFieldRuntime(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    fireEvent.click(screen.getByTestId("new-agent-deck-list").querySelector(`[data-deck-id="${FIXTURE_DAEMON_ID}"]`)!);
    await flush();
    await flush();

    hold();
    voice.deliver("deck build box");
    await completeUtterance();
    await chooseBilling();
    release();
    await flush();
    await flush();

    expect(chosenDeck()).toBe(FIXTURE_DAEMON_ID);
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/billing");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIALOG_MOVED_ON);
  });

  /**
   * Scenario: with the form live on the local deck, say "mode dispatcher";
   * while it is being worked out, click the other deck. Changing the deck
   * mid-flight takes the form down, which is the context change the pending
   * answer's check exists to catch: the Mode is not set on a form the answer
   * was never about.
   */
  it("refuses a fill answer judged before the deck changed by hand", async () => {
    const voice = microphone([]);
    const { deck, hold, release } = deckFieldRuntime(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    fireEvent.click(screen.getByTestId("new-agent-deck-list").querySelector(`[data-deck-id="${FIXTURE_DAEMON_ID}"]`)!);
    await flush();
    await flush();
    await chooseBilling();

    hold();
    voice.deliver("mode dispatcher");
    await completeUtterance();
    fireEvent.click(screen.getByTestId("new-agent-deck-list").querySelector(`[data-deck-id="${FIXTURE_REMOTE_DAEMON_ID}"]`)!);
    await flush();
    await flush();
    release();
    await flush();
    await flush();

    expect(chosenDeck()).toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(pressedMode()).toBe("none");
    expect(screen.getByTestId("voice-report")).toHaveTextContent(DIALOG_MOVED_ON);
  });

  /**
   * Scenario (#1247): fill the form and say "close" — the dialog closes and
   * opening it again restores the form. Then say "discard": the dialog closes
   * and opening it again is a fresh form.
   */
  it("keeps the form on a spoken close and forgets it on a spoken discard", async () => {
    const voice = microphone([]);
    const { deck } = deckFieldRuntime(voice);
    render(<DeckShell runtime={deck} initialView={{ kind: "overview" }} />);
    await turnVoiceOn();
    await openDialog();
    fireEvent.click(screen.getByTestId("new-agent-deck-list").querySelector(`[data-deck-id="${FIXTURE_DAEMON_ID}"]`)!);
    await flush();
    await flush();
    await chooseBilling();
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "mine" } });

    voice.deliver("close");
    await completeUtterance();
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    await openDialog();
    await flush();
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/billing");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("mine");
    expect(screen.getByTestId("new-agent-restored")).toHaveTextContent(DRAFT_RESTORED);

    voice.deliver("discard");
    await completeUtterance();
    expect(screen.queryByTestId("new-agent-dialog")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Discarded the New agent form.");
    await openDialog();
    expect(chosenDeck()).toBeUndefined();
    expect(screen.getByTestId("new-agent-name")).toHaveValue("");
    expect(screen.queryByTestId("new-agent-restored")).toBeNull();
  });
});
