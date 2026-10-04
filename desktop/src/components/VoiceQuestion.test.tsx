/**
 * PRD #1542 — `question/desktop/005` and `006`: the voice panel answering the
 * question the agent on screen is waiting on. The Rust half (which option an
 * utterance picks, the form, every sentence) is stubbed at the runtime seam;
 * what is asserted here is what the panel does with it — the "always allow"
 * confirmation, the countdown, and calling the countdown off when the
 * question it was for is gone.
 */
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fixtureDesktopFeatures, type VoiceResultDto, type VoiceStatusDto, type VoiceTranscriptionDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import {
  QUESTION_ALWAYS_DECLINED,
  QUESTION_MOVED_ON,
  questionLost,
  type AnswerOutcomeDto,
  type PendingQuestionDto,
  type QuestionResultDto,
  type QuestionSelectionDto,
  type VoiceQuestionTarget,
} from "../lib/voiceQuestion";
import { VOICE_DICTATION_SEND_MS, VOICE_STATUS_POLL_MS, VoiceControlPanel, type VoicePane } from "./VoiceControlPanel";

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

/** A Claude Code permission prompt for `touch x`. */
const PERMISSION: PendingQuestionDto = {
  id: "q-1",
  kind: "permission",
  channel: "held",
  answerable: true,
  tool: { name: "Bash", detail: "touch x" },
  questions: [{
    prompt: "Allow Bash?",
    multiSelect: false,
    options: [
      { index: 1, label: "Yes", role: "allow_once", answerable: true },
      { index: 2, label: "Yes, and don't ask again", role: "allow_always", answerable: true, scope: "commands matching `touch *`" },
      { index: 3, label: "No", role: "deny", answerable: true },
    ],
  }],
};

const ALWAYS = "This will always allow commands matching `touch *`. Confirm?";

function pane(question: PendingQuestionDto | undefined = PERMISSION): VoicePane {
  return { deckId: "local", agentId: "a1", label: "tester", spawnedAtMs: 1, question };
}

/** What Rust answers for each utterance, as `desktop_voice_question` would. */
function questionAnswers(said: string): QuestionResultDto {
  const answered = (form: QuestionSelectionDto[], summary: string, always?: string): QuestionResultDto => ({
    verdict: { kind: "answered", form, complete: true, summary, ...(always ? { always } : {}) },
    resolveMs: 12,
    backend: "stub",
  });
  if (said === "yes") return answered([{ questionIndex: 0, optionIndices: [1] }], "Allow once — touch x");
  if (said === "always") return answered([{ questionIndex: 0, optionIndices: [2] }], "Always allow — commands matching `touch *`", ALWAYS);
  return { verdict: { kind: "not_answer" }, resolveMs: 12, backend: "stub" };
}

function runtime(voice: ReturnType<typeof microphone>, sent: (form: QuestionSelectionDto[], confirmed: boolean) => AnswerOutcomeDto) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => ({
    backend: "stub", resolveMs: 21,
    outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` },
  }));
  const resolveVoiceQuestion = vi.fn(async (_target: VoiceQuestionTarget, utterance: string) => questionAnswers(utterance));
  const sendVoiceAnswer = vi.fn(async (_target: VoiceQuestionTarget, form: QuestionSelectionDto[], confirmed: boolean) => sent(form, confirmed));
  return {
    rt: {
      desktopFeatures: fixtureDesktopFeatures(),
      declareVoiceScreen: vi.fn(),
      sendTerminalInput: vi.fn(async () => undefined),
      resolveVoice,
      resolveVoiceQuestion,
      sendVoiceAnswer,
      ...voice,
    } as unknown as DeckRuntimeState,
    resolveVoice,
    resolveVoiceQuestion,
    sendVoiceAnswer,
  };
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

const SENT: AnswerOutcomeDto = { kind: "answered", sentence: "Allowed: touch x" };

describe("answering an agent's question by voice", () => {
  beforeEach(() => { window.localStorage.clear(); vi.useFakeTimers(); });
  afterEach(() => vi.useRealTimers());

  /**
   * Scenario (question/desktop/005): with a permission prompt on screen, the
   * user says "always". The panel shows a confirmation naming what "always"
   * covers and starts no countdown; Cancel sends nothing. Said again and
   * confirmed — by the button, and then by saying "confirm" — the five-second
   * countdown runs and the answer goes out asserting the confirmation.
   */
  it("question/desktop/005: always allow is sent only after a confirmation naming its scope", async () => {
    const voice = microphone();
    const { rt, sendVoiceAnswer, resolveVoiceQuestion } = runtime(voice, (_form, confirmed) => ({ kind: "answered", sentence: confirmed ? "Always allowed: commands matching `touch *`" : "unconfirmed" }));
    render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();

    await speak(voice, "always");
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveTextContent(ALWAYS);
    expect(screen.getByTestId("voice-question")).toHaveTextContent("waiting for your confirmation");
    expect(screen.getByTestId("voice-question")).not.toHaveTextContent("sending in");
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await flush();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(QUESTION_ALWAYS_DECLINED);
    await waitOut(VOICE_DICTATION_SEND_MS + 1_000);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();

    // Confirmed by the button: the countdown, then the send, confirmed.
    await speak(voice, "always");
    fireEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Confirm" }));
    await flush();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.getByTestId("voice-question")).toHaveTextContent("Always allow — commands matching `touch *` — sending in 5 s.");
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
    expect(sendVoiceAnswer).toHaveBeenCalledWith(
      { deckId: "local", agentId: "a1", agent: "tester", questionId: "q-1" },
      [{ questionIndex: 0, optionIndices: [2] }],
      true,
    );
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Always allowed: commands matching `touch *`");

    // Confirmed by voice: "confirm" is the dialog's, never sent to the model.
    await speak(voice, "always");
    const asked = resolveVoiceQuestion.mock.calls.length;
    await speak(voice, "confirm");
    expect(resolveVoiceQuestion.mock.calls.length).toBe(asked);
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(2);
    expect(sendVoiceAnswer.mock.calls[1][2]).toBe(true);
  });

  /**
   * Scenario (question/desktop/006): the user says "yes" and the countdown to
   * "Allow once — touch x" starts. Before it runs out the agent's question
   * changes (answered by keyboard, or replaced), so the countdown stops and the
   * panel says nothing was sent. Each refusal the deck can send back is shown
   * as its own plain sentence, and an utterance that is not an answer goes on
   * to ordinary command handling.
   */
  it("question/desktop/006: the countdown is called off when the question changes, and every refusal reads as a sentence", async () => {
    const voice = microphone();
    const { rt, sendVoiceAnswer, resolveVoice } = runtime(voice, () => SENT);
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();

    await speak(voice, "yes");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("Allow once — touch x — sending in 5 s.");
    await waitOut(2_000);
    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane({ ...PERMISSION, id: "q-2" })} selectedDeckId="local" />);
    await flush();
    expect(screen.queryByTestId("voice-question")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(QUESTION_MOVED_ON);
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();

    // Not an answer: it goes on to the command resolver.
    await speak(voice, "go to the overview");
    expect(resolveVoice).toHaveBeenCalledWith("go to the overview");
    view.unmount();
  });

  it("question/desktop/006: each refusal's sentence is what the row says", async () => {
    const refusals: AnswerOutcomeDto[] = [
      { kind: "refused", code: "no_pending_question", sentence: "No question is waiting in this agent." },
      { kind: "refused", code: "stale", sentence: QUESTION_MOVED_ON },
      { kind: "refused", code: "invalid_answer", sentence: "I couldn't match that to the options: Yes, No." },
      { kind: "refused", code: "keyboard_only", sentence: "That option has to be answered by keyboard." },
      { kind: "refused", code: "unsupported", sentence: "tester's questions have to be answered by keyboard." },
      { kind: "refused", code: "channel_gone", sentence: "The agent stopped waiting for that answer — nothing was sent." },
      { kind: "refused", code: "always_not_confirmed", sentence: "Always allow was not confirmed — nothing was sent." },
      { kind: "refused", code: "write_failed", sentence: "Couldn't send the answer: pane closed." },
      { kind: "withheld", sentence: "This deck cannot answer questions by voice — update the deck to use it. Nothing was sent." },
      SENT,
    ];
    for (const outcome of refusals) {
      const voice = microphone();
      const { rt, sendVoiceAnswer } = runtime(voice, () => outcome);
      const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
      await turnOnVoice();
      await speak(voice, "yes");
      await waitOut(VOICE_DICTATION_SEND_MS);
      expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
      expect(screen.getByTestId("voice-report")).toHaveTextContent(outcome.sentence);
      expect(screen.queryByTestId("voice-question")).toBeNull();
      view.unmount();
    }
  });

  /** The staleness rule itself, case by case. */
  it("question/desktop/006: questionLost names why a countdown no longer holds", () => {
    const aim = { deckId: "local", agentId: "a1", questionId: "q-1", spawnedAtMs: 1, deck: "local" };
    const now = { pane: pane(), confirmation: false, deck: "local" };
    expect(questionLost(aim, now)).toBeUndefined();
    expect(questionLost(aim, { ...now, pane: pane({ ...PERMISSION, id: "q-2" }) })?.code).toBe("question");
    expect(questionLost(aim, { ...now, pane: { ...pane(), question: undefined } })?.code).toBe("question");
    expect(questionLost(aim, { ...now, pane: { ...pane(), agentId: "a2" } })?.code).toBe("pane");
    expect(questionLost(aim, { ...now, pane: undefined })?.code).toBe("pane");
    expect(questionLost(aim, { ...now, pane: { ...pane(), spawnedAtMs: 2 } })?.code).toBe("replaced");
    expect(questionLost(aim, { ...now, confirmation: true })?.code).toBe("confirmation");
    expect(questionLost(aim, { ...now, deck: "remote" })?.code).toBe("deck");
  });
});
