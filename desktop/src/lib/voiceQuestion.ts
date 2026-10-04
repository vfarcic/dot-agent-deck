/**
 * PRD #1542 — answering, by voice, the question the agent on screen is
 * waiting on: the wire shapes `desktop_voice_question` and
 * `desktop_voice_answer_question` speak, and the one rule the panel applies
 * on its own — whether a running answer countdown still holds.
 *
 * Every sentence the user reads comes from Rust (`voice/question.rs`), except
 * the countdown's own called-off sentences below, which are about state only
 * the webview holds.
 */

/** One option of a pending question, as the desktop crate projects it. Every text field is the agent's own, already scrubbed. */
export type PendingQuestionOptionDto = {
  /** 1-based, the number the agent shows beside it. */
  index: number;
  label: string;
  description?: string;
  /** `allow_once`, `allow_always`, `deny`, `choice`, `free_text` or `unknown`. */
  role: string;
  /** Whether the deck can send this option. */
  answerable: boolean;
  /** For `allow_always`: what "always" covers. */
  scope?: string;
};

/** One question of a pending question. */
export type PendingQuestionItemDto = {
  prompt: string;
  header?: string;
  multiSelect: boolean;
  options: PendingQuestionOptionDto[];
};

/** `SessionSnapshot.pending_question`, as the desktop crate projects it (`DesktopPendingQuestion`). */
export type PendingQuestionDto = {
  /** What an answer names, and what the countdown's staleness check compares. */
  id: string;
  kind: string;
  channel: string;
  questions: PendingQuestionItemDto[];
  tool?: { name: string; detail?: string };
  /** Whether the deck can answer this question at all. */
  answerable: boolean;
  /** The daemon's revision of this question (audit A4): which registration of it an answer is bound to. */
  revision?: number;
};

/** One question's answer as the form holds it. */
export type QuestionSelectionDto = { questionIndex: number; optionIndices: number[]; text?: string };

/** A free-text option the form is waiting for the words of. */
export type QuestionTextSlotDto = { questionIndex: number; optionIndex: number };

/** What one utterance did to the question (`voice::question::QuestionVerdict`). */
export type QuestionVerdictDto =
  | {
    kind: "answered";
    /** The WHOLE form after this utterance — it replaces the panel's copy. */
    form: QuestionSelectionDto[];
    complete: boolean;
    awaitingText?: QuestionTextSlotDto;
    /** The "always allow" confirmation to show before the countdown. */
    always?: string;
    /** The form in words — the countdown line once complete. */
    summary: string;
  }
  | { kind: "cancelled"; sentence: string }
  | { kind: "not_answer" }
  | { kind: "refused"; sentence: string };

/** One utterance's verdict, with what the backend cost. */
export type QuestionResultDto = { verdict: QuestionVerdictDto; resolveMs: number | null; backend: string };

/** What sending a form came to (`voice::question::AnswerOutcome`). */
export type AnswerOutcomeDto = {
  /** `cancelled`: the panel's lease was cancelled before the answer went; `too_late`: after. */
  kind: "answered" | "refused" | "withheld" | "superseded" | "cancelled" | "too_late";
  /** The daemon's refusal code, for a refusal. */
  code?: string;
  sentence: string;
};

/** Which question an utterance or a send is about: the agent by its composite identity, the name its pane shows, and the question id — and revision, audit A4 — it saw. */
export type VoiceQuestionTarget = { deckId: string; agentId: string; agent: string; questionId: string; revision?: number };

/** The countdown was running for a question that is no longer the one waiting. */
export const QUESTION_MOVED_ON = "That question was answered or changed before I could send it — nothing was sent.";
/** Said after an answer's outcome when the cancel asked for while it was on its way never reached the app (audit A8). */
export const QUESTION_CANCEL_FAILED = "The cancel did not reach the app, so it could not stop the answer.";
/** The user called the countdown off. */
export const QUESTION_ANSWER_CANCELLED = "Answer cancelled — nothing was sent.";
/** The "always allow" confirmation was declined or closed. */
export const QUESTION_ALWAYS_DECLINED = "Always allow was not confirmed — nothing was sent.";

/** While the "always allow" confirmation is open, anything but its own words. */
export const QUESTION_SAY_CONFIRM = "Say “confirm” to always allow, or “cancel”.";
/** The microphone heard speech while the answer was counting down (audit A3). */
export const QUESTION_COUNTDOWN_STOPPED = " — stopped because you spoke. Say the answer again to send it.";

/** Any other reason a running answer countdown was called off. */
export function questionCalledOff(why: string): string {
  return `Nothing was sent — ${why}.`;
}

/** The pane on screen, as far as a question countdown needs it. `terminalHidden`: another tab of the pane is showing, so the agent's prompt is not on screen. */
export type QuestionPane = { deckId: string; agentId: string; spawnedAtMs?: number; terminalHidden?: boolean; question?: PendingQuestionDto };

/** Why {@link questionLost} called a countdown off: a code, and the reason in words. */
export type QuestionLost = { code: "question" | "pane" | "replaced" | "hidden" | "confirmation" | "deck"; why: string };

/**
 * PRD #1542 — the gate a running answer countdown is held to, on every
 * context change and immediately before the answer is sent. It answers
 * `undefined` while the answer still holds, or why not: the question waiting
 * is no longer the one answered (`question`), the pane on screen shows another
 * agent or none (`pane`), the agent was replaced under the same id
 * (`replaced`), the pane shows another tab so the prompt being answered is not
 * on screen (`hidden`, audit A7), a confirmation is open (`confirmation`), or
 * the selected deck changed (`deck`).
 */
export function questionLost(
  aim: { deckId: string; agentId: string; questionId: string; revision?: number; spawnedAtMs?: number; deck?: string },
  now: { pane?: QuestionPane; confirmation: boolean; deck?: string },
): QuestionLost | undefined {
  const pane = now.pane;
  if (!pane || pane.deckId !== aim.deckId || pane.agentId !== aim.agentId) return { code: "pane", why: pane ? "the pane on screen changed" : "the pane closed" };
  if (aim.spawnedAtMs !== undefined && pane.spawnedAtMs !== undefined && aim.spawnedAtMs !== pane.spawnedAtMs) return { code: "replaced", why: "the agent in the pane was replaced" };
  if (pane.terminalHidden) return { code: "hidden", why: "its terminal is not shown" };
  if (pane.question?.id !== aim.questionId) return { code: "question", why: "the question changed" };
  if (aim.revision !== undefined && pane.question.revision !== aim.revision) return { code: "question", why: "the question changed" };
  if (now.confirmation) return { code: "confirmation", why: "a confirmation is open" };
  if (now.deck !== aim.deck) return { code: "deck", why: "the deck changed" };
  return undefined;
}

/** The sentence for a countdown {@link questionLost} called off. */
export function questionLostSentence(lost: QuestionLost): string {
  return lost.code === "question" ? QUESTION_MOVED_ON : questionCalledOff(lost.why);
}

/**
 * Whether `utterance` is, whole, a confirmation of the "always allow" dialog:
 * the dedicated word "confirm", or "always allow" said again (audit A1). A
 * bare "yes", "sure" or "ok" is NOT one — a lasting grant must not ride on the
 * most generic affirmative there is.
 */
export function saysConfirm(utterance: string): boolean {
  return ["confirm", "confirm it", "yes confirm", "i confirm", "confirmed", "always allow", "always allow it", "yes always allow"].includes(spoken(utterance));
}

/** Whether `utterance` is, whole, a refusal of the "always allow" dialog or of a running countdown. */
export function saysCancel(utterance: string): boolean {
  return ["cancel", "cancel that", "never mind", "nevermind", "no", "stop"].includes(spoken(utterance));
}

function spoken(utterance: string): string {
  return utterance.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, " ").trim();
}
