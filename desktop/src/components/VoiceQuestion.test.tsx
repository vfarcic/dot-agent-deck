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
  QUESTION_ANSWER_CANCELLED,
  QUESTION_CANCEL_FAILED,
  QUESTION_COUNTDOWN_STOPPED,
  QUESTION_MOVED_ON,
  QUESTION_SAY_CONFIRM,
  questionLost,
  saysConfirm,
  type AnswerOutcomeDto,
  type PendingQuestionDto,
  type QuestionResultDto,
  type QuestionSelectionDto,
  type VoiceQuestionTarget,
} from "../lib/voiceQuestion";
import { VOICE_DICTATION_SEND_MS, VOICE_JOIN_WINDOW_MS, VOICE_STATUS_POLL_MS, VoiceControlPanel, type VoicePane } from "./VoiceControlPanel";

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

function runtime(voice: Pick<ReturnType<typeof microphone>, "voiceStart" | "voiceStatus" | "voiceStop" | "voiceCancel">, sent: (form: QuestionSelectionDto[], confirmed: boolean) => AnswerOutcomeDto | Promise<AnswerOutcomeDto>) {
  const resolveVoice = vi.fn(async (transcript: string): Promise<VoiceResultDto> => ({
    backend: "stub", resolveMs: 21,
    outcome: { kind: "no_match", transcript, sentence: `Heard: “${transcript}” — no matching action.` },
  }));
  const resolveVoiceQuestion = vi.fn(async (_target: VoiceQuestionTarget, utterance: string) => questionAnswers(utterance));
  const sendVoiceAnswer = vi.fn(async (_target: VoiceQuestionTarget, form: QuestionSelectionDto[], confirmed: boolean, _lease: string) => sent(form, confirmed));
  const cancelVoiceAnswer = vi.fn(async (_lease: string) => undefined);
  return {
    rt: {
      desktopFeatures: fixtureDesktopFeatures(),
      declareVoiceScreen: vi.fn(),
      sendTerminalInput: vi.fn(async () => undefined),
      resolveVoice,
      resolveVoiceQuestion,
      sendVoiceAnswer,
      cancelVoiceAnswer,
      ...voice,
    } as unknown as DeckRuntimeState,
    resolveVoice,
    resolveVoiceQuestion,
    sendVoiceAnswer,
    cancelVoiceAnswer,
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
      expect.any(String),
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

  /**
   * Scenario (question/desktop/006, audit R5): the question keeps its id but
   * the deck registers it again — a new revision — while nothing else about
   * the pane changes. A countdown is called off at once with nothing sent, and
   * an answer already on its way has its lease cancelled at once.
   */
  it("question/desktop/006: the same question id at a new revision calls the answer off at once", async () => {
    const voice = microphone();
    let deliver: ((outcome: AnswerOutcomeDto) => void) | undefined;
    const { rt, sendVoiceAnswer, cancelVoiceAnswer } = runtime(voice, () => new Promise<AnswerOutcomeDto>((resolve) => { deliver = resolve; }));
    const first = pane({ ...PERMISSION, revision: 7 });
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={first} selectedDeckId="local" />);
    await turnOnVoice();

    await speak(voice, "yes");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    await waitOut(2_000);
    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane({ ...PERMISSION, revision: 8 })} selectedDeckId="local" />);
    await flush();
    expect(screen.queryByTestId("voice-question")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(QUESTION_MOVED_ON);
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();

    await speak(voice, "yes");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
    const lease = sendVoiceAnswer.mock.calls[0][3];
    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane({ ...PERMISSION, revision: 9 })} selectedDeckId="local" />);
    await flush();
    expect(cancelVoiceAnswer).toHaveBeenCalledWith(lease);
    expect(screen.getByTestId("voice-question")).toHaveTextContent("cancelling…");
    deliver?.({ kind: "cancelled", sentence: "Answer cancelled" });
    await flush();
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
    /* The same id registered again is another question (audit A4). */
    const revised = { ...aim, revision: 7 };
    expect(questionLost(revised, { ...now, pane: pane({ ...PERMISSION, revision: 7 }) })).toBeUndefined();
    expect(questionLost(revised, { ...now, pane: pane({ ...PERMISSION, revision: 8 }) })?.code).toBe("question");
  });

  /**
   * Scenario (question/desktop/008, audit A1): with the "always allow"
   * confirmation open, the user says "yes". That is not a confirmation: the
   * dialog stays open, the row says to say "confirm", and no countdown starts.
   * Saying "always allow" again confirms it.
   */
  it("question/desktop/008: a bare yes does not confirm always allow", async () => {
    const voice = microphone();
    const { rt, sendVoiceAnswer, resolveVoiceQuestion } = runtime(voice, () => SENT);
    render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();
    await speak(voice, "always");
    expect(screen.getByRole("alertdialog")).toHaveTextContent(ALWAYS);
    const asked = resolveVoiceQuestion.mock.calls.length;
    for (const word of ["yes", "sure", "ok", "yes please"]) {
      await speak(voice, word);
      expect(screen.getByRole("alertdialog")).toBeInTheDocument();
      expect(screen.getByTestId("voice-report")).toHaveTextContent(QUESTION_SAY_CONFIRM);
      expect(screen.getByTestId("voice-question")).not.toHaveTextContent("sending in");
    }
    expect(resolveVoiceQuestion.mock.calls.length).toBe(asked);
    await waitOut(VOICE_DICTATION_SEND_MS + 1_000);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    await speak(voice, "always allow");
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    expect(saysConfirm("yes")).toBe(false);
    expect(saysConfirm("Confirm.")).toBe(true);
  });

  /**
   * Scenario (question/desktop/009, audit A3): the countdown to "Allow once"
   * is a second from its end when the user starts saying "cancel". The
   * microphone reports speech, and the countdown stops at once — though
   * transcribing and working out the words then take far longer than the
   * second that was left — so nothing is sent. Likewise when the words come
   * back unreadable or refused: the countdown stays stopped until a fresh,
   * complete answer re-arms it.
   */
  it("question/desktop/009: speaking stops the answer countdown before the words are worked out", async () => {
    let speaking = false;
    let ended = false;
    let release: ((text: string) => void) | undefined;
    const idle = (state: VoiceStatusDto["state"]): VoiceStatusDto => ({ state, capturedMs: 900, maxMs: 30_000, capped: false, speech: speaking, available: true, backend: "remote" });
    const voice = {
      voiceStart: vi.fn(async () => idle("recording")),
      voiceStatus: vi.fn(async () => idle(ended ? "done" : "recording")),
      voiceStop: vi.fn(() => new Promise<VoiceTranscriptionDto>((resolve) => {
        release = (transcript) => resolve({ outcome: { kind: "heard", transcript, sentence: `Heard: “${transcript}”.` }, transcribeMs: 11, backend: "stub", audioMs: 900 });
      })),
      voiceCancel: vi.fn(async () => idle("idle")),
    };
    /** One utterance, its transcription released straight away. */
    const utter = async (words: string) => {
      speaking = true;
      ended = true;
      await waitOut(VOICE_STATUS_POLL_MS);
      speaking = false;
      ended = false;
      release?.(words);
      await flush();
    };
    const { rt, sendVoiceAnswer, resolveVoiceQuestion } = runtime(voice, () => SENT);
    let resolveLater: ((result: QuestionResultDto) => void) | undefined;
    resolveVoiceQuestion.mockImplementation(async (_target: VoiceQuestionTarget, said: string) => {
      if (said === "cancel" || said === "mumble") return new Promise<QuestionResultDto>((resolve) => { resolveLater = resolve; });
      if (said === "garbled") throw new Error("the command backend answered the question unreadably");
      return questionAnswers(said);
    });
    render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();
    await utter("yes");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    await waitOut(VOICE_DICTATION_SEND_MS - 1_000);

    // Speech starts a second before the end: stopped on the spot.
    speaking = true;
    await waitOut(VOICE_STATUS_POLL_MS);
    expect(screen.getByTestId("voice-question")).toHaveTextContent(QUESTION_COUNTDOWN_STOPPED.trim());
    // The words take far longer than the second that was left.
    ended = true;
    await waitOut(VOICE_STATUS_POLL_MS);
    speaking = false;
    ended = false;
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    release?.("cancel");
    await flush();
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    resolveLater?.({ verdict: { kind: "cancelled", sentence: "Answer cancelled — nothing was sent." }, resolveMs: 9000, backend: "stub" });
    await flush();
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();

    // A failed resolve leaves it disarmed; a refusal too.
    await utter("yes");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    await utter("garbled");
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    await utter("mumble");
    resolveLater?.({ verdict: { kind: "refused", sentence: "Heard: “mumble” — I couldn't tell which option that chooses, so nothing was chosen." }, resolveMs: 12, backend: "stub" });
    await flush();
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    expect(screen.getByTestId("voice-question")).toHaveTextContent(QUESTION_COUNTDOWN_STOPPED.trim());

    // A fresh complete answer arms it again.
    await utter("yes");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario (question/desktop/010, audit A7): the agent's pane is showing its
   * Diff tab, so the prompt is not on screen. "Yes" is not offered to the
   * question at all and goes on as an ordinary command. With the terminal
   * showing, "yes" starts the countdown; switching to another tab before it
   * ends calls it off and nothing is sent.
   */
  it("question/desktop/010: an answer needs the agent's terminal on screen", async () => {
    const voice = microphone();
    const { rt, sendVoiceAnswer, resolveVoiceQuestion, resolveVoice } = runtime(voice, () => SENT);
    const hidden = { ...pane(), terminalHidden: true };
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={hidden} selectedDeckId="local" />);
    await turnOnVoice();
    await speak(voice, "yes");
    expect(resolveVoiceQuestion).not.toHaveBeenCalled();
    expect(resolveVoice).toHaveBeenCalledWith("yes");
    expect(screen.queryByTestId("voice-question")).toBeNull();
    // An unmatched command is held for the rest of its sentence; let it go.
    await waitOut(VOICE_JOIN_WINDOW_MS + 1_000);

    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await flush();
    await speak(voice, "yes");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending in 5 s");
    await waitOut(2_000);
    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={hidden} selectedDeckId="local" />);
    await flush();
    expect(screen.queryByTestId("voice-question")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("its terminal is not shown");
    await waitOut(VOICE_DICTATION_SEND_MS * 2);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
    expect(questionLost(
      { deckId: "local", agentId: "a1", questionId: "q-1", spawnedAtMs: 1, deck: "local" },
      { pane: hidden, confirmation: false, deck: "local" },
    )?.code).toBe("hidden");
  });

  /**
   * Scenario (question/desktop/011, audit A8): the countdown runs out and the
   * answer is on its way, but the deck is slow to take it. The row keeps the
   * answer and its Cancel while it is sending. The user changes pane: the
   * lease the answer was sent under is cancelled at once, and the row reports
   * what Rust says became of it — here, too late to cancel.
   */
  it("question/desktop/011: an answer on its way is cancelled by its lease and its outcome is always told", async () => {
    const voice = microphone();
    let deliver: ((outcome: AnswerOutcomeDto) => void) | undefined;
    const { rt, sendVoiceAnswer, cancelVoiceAnswer } = runtime(voice, () => new Promise<AnswerOutcomeDto>((resolve) => { deliver = resolve; }));
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();
    await speak(voice, "yes");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
    const lease = sendVoiceAnswer.mock.calls[0][3];
    expect(lease).toMatch(/^[A-Za-z0-9-]{1,64}$/);
    expect(screen.getByTestId("voice-question")).toHaveTextContent("sending…");
    expect(screen.getByTestId("voice-question-cancel")).toBeInTheDocument();

    view.rerender(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={{ ...pane(), agentId: "a2" }} selectedDeckId="local" />);
    await flush();
    expect(cancelVoiceAnswer).toHaveBeenCalledWith(lease);
    expect(screen.getByTestId("voice-question")).toHaveTextContent("cancelling…");
    deliver?.({ kind: "too_late", sentence: "Too late to cancel — Allowed: touch x" });
    await flush();
    expect(screen.queryByTestId("voice-question")).toBeNull();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Too late to cancel — Allowed: touch x");
    view.unmount();
  });

  /**
   * Scenario (question/desktop/012, audit A1 — an accepted residual): the
   * model picks "Allow once" for words that do not mean it, quoting them
   * exactly — "what time is it", and the "run that" inside "no don't run
   * that". The app cannot tell, so the answer is armed; what the countdown
   * shows is exactly what would be sent, and pressing Cancel, or speaking
   * ("cancel"), stops it with nothing sent.
   */
  it("question/desktop/012: an approval the model ties to words that do not mean it is shown before it is sent, and cancelling sends nothing", async () => {
    const voice = microphone();
    const { rt, sendVoiceAnswer, resolveVoiceQuestion } = runtime(voice, () => ({ kind: "answered", sentence: "Allowed: touch x" }));
    resolveVoiceQuestion.mockImplementation(async (_target: VoiceQuestionTarget, utterance: string) =>
      utterance === "what time is it" || utterance === "no don't run that"
        ? questionAnswers("yes")
        : questionAnswers(utterance));
    render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();

    await speak(voice, "what time is it");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("Allow once — touch x — sending in 5 s.");
    fireEvent.click(screen.getByTestId("voice-question-cancel"));
    await flush();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(QUESTION_ANSWER_CANCELLED);
    await waitOut(VOICE_DICTATION_SEND_MS + 1_000);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();

    await speak(voice, "no don't run that");
    expect(screen.getByTestId("voice-question")).toHaveTextContent("Allow once — touch x — sending in 5 s.");
    await speak(voice, "cancel");
    expect(screen.getByTestId("voice-question")).toHaveTextContent(`Allow once — touch x${QUESTION_COUNTDOWN_STOPPED}`);
    await waitOut(VOICE_DICTATION_SEND_MS + 1_000);
    expect(sendVoiceAnswer).not.toHaveBeenCalled();
  });

  /**
   * Scenario (question/desktop/011, audit A8): the answer is on its way and
   * the user presses Cancel, but the cancel itself never reaches the app. The
   * answer goes through, and the row says so — and says the cancel could not
   * stop it — rather than reading as if nothing had been asked.
   */
  it("question/desktop/011: a cancel that fails to reach the app is reported with the outcome", async () => {
    const voice = microphone();
    let deliver: ((outcome: AnswerOutcomeDto) => void) | undefined;
    const { rt, sendVoiceAnswer, cancelVoiceAnswer } = runtime(voice, () => new Promise<AnswerOutcomeDto>((resolve) => { deliver = resolve; }));
    cancelVoiceAnswer.mockRejectedValueOnce(new Error("ipc down"));
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();
    await speak(voice, "yes");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByTestId("voice-question-cancel"));
    await flush();
    expect(cancelVoiceAnswer).toHaveBeenCalledTimes(1);
    deliver?.({ kind: "answered", sentence: "Allowed: touch x" });
    await flush();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(`Allowed: touch x ${QUESTION_CANCEL_FAILED}`);
    view.unmount();
  });

  /**
   * Scenario (question/desktop/011, audit R5): the same failed cancel, but
   * this time the answer's outcome arrives first and the cancel's failure
   * only afterwards. The row first says what became of the answer, then is
   * amended to say the cancel could not stop it.
   */
  it("question/desktop/011: a cancel that fails after the outcome arrived still amends the row", async () => {
    const voice = microphone();
    let deliver: ((outcome: AnswerOutcomeDto) => void) | undefined;
    let failCancel: ((cause: Error) => void) | undefined;
    const { rt, sendVoiceAnswer, cancelVoiceAnswer } = runtime(voice, () => new Promise<AnswerOutcomeDto>((resolve) => { deliver = resolve; }));
    cancelVoiceAnswer.mockImplementationOnce(() => new Promise((_, reject) => { failCancel = reject; }));
    const view = render(<VoiceControlPanel runtime={rt} screen="agent" onDispatch={() => undefined} pane={pane()} selectedDeckId="local" />);
    await turnOnVoice();
    await speak(voice, "yes");
    await waitOut(VOICE_DICTATION_SEND_MS);
    expect(sendVoiceAnswer).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByTestId("voice-question-cancel"));
    await flush();
    expect(cancelVoiceAnswer).toHaveBeenCalledTimes(1);
    deliver?.({ kind: "answered", sentence: "Allowed: touch x" });
    await flush();
    expect(screen.getByTestId("voice-report")).toHaveTextContent("Allowed: touch x");
    expect(screen.getByTestId("voice-report")).not.toHaveTextContent(QUESTION_CANCEL_FAILED);
    failCancel?.(new Error("ipc down"));
    await flush();
    expect(screen.getByTestId("voice-report")).toHaveTextContent(`Allowed: touch x ${QUESTION_CANCEL_FAILED}`);
    view.unmount();
  });
});
