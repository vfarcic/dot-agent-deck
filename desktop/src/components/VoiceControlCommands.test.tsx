import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import {
  DEFAULT_DESKTOP_SETTINGS,
  type DesktopSettingsDto,
  type VoiceCommandDto,
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
import {
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

describe("what can I say?", () => {
  beforeEach(() => {
    window.localStorage.clear();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const VOCABULARY: VoiceCommandDto[] = [
    { id: "open_overview", description: "Show every agent in one list.", callable: true, unavailable_hint: "the agent overview opens from the deck", params: [] },
    { id: "voice_off", description: "Stop listening.", callable: true, unavailable_hint: "turning voice off works anywhere", params: [] },
    { id: "open_deck", description: "Go back to the terminals.", callable: false, unavailable_hint: "returning to the deck works from the agent overview", params: [] },
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

    expect(voiceCommands).toHaveBeenCalledWith("deck");
    const overlay = screen.getByTestId("voice-help");
    expect(overlay).toHaveTextContent("Show every agent in one list.");
    const here = overlay.querySelector('[data-where="here"]');
    const elsewhere = overlay.querySelector('[data-where="elsewhere"]');
    expect(Array.from(here?.querySelectorAll("[data-command]") ?? []).map((row) => row.getAttribute("data-command")))
      .toEqual(["open_overview", "voice_off"]);
    expect(Array.from(elsewhere?.querySelectorAll("[data-command]") ?? []).map((row) => row.getAttribute("data-command")))
      .toEqual(["open_deck"]);
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

    expect(voiceCommands).toHaveBeenCalledWith("overview");
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
    const resolveVoice: ResolveVoice = vi.fn(async () => dispatch("open_overview", "openOverview", "Opening the agent overview.", "show me every agent"));
    render(<DeckShell runtime={runtime(resolveVoice, voice)} />);

    await turnVoiceOn();
    expect(screen.getByTestId("voice-hint")).toBeInTheDocument();

    await completeUtterance();
    expect(screen.queryByTestId("voice-hint")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening the agent overview.");

    // Off and on again: `turnOn` clears the row through `forget()`, which is
    // the user-visible route back to an empty one.
    await act(async () => { fireEvent.click(voiceButton()); });
    await act(async () => { fireEvent.click(voiceButton()); });
    await flush();
    expect(screen.getByTestId("voice-hint")).toBeInTheDocument();
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent("Opening the agent overview.");
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
  /** What the deck itself calls that agent — read from the fixture, not retyped. */
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
        : (answers[utterance] ?? dispatch("open_overview", "openOverview", "Opening the agent overview.", utterance)));
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
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Opening the agent overview.");
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
   * Scenario: with no overlay up, the same word closes the agent's pane. The
   * precedence is decided at dispatch because *"an overlay is open"* is not a
   * screen — and this is the "otherwise" half of it.
   */
  it("closes the agent view when no overlay is up", async () => {
    const voice = microphone(["open the planner"]);
    const deck = closingDeck(voice);
    render(<DeckShell runtime={deck} />);

    await turnVoiceOn();
    await completeUtterance();
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();

    voice.deliver("close this");
    await completeUtterance();

    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
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
