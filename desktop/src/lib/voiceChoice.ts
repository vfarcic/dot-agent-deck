/**
 * PRD #1261 — answering a numbered voice choice.
 *
 * When a spoken name matched several things, Rust's `param_ambiguous` outcome
 * carries the candidates and the voice panel offers them as a numbered list.
 * The next utterance is answered LOCALLY — no Commands backend call — by
 * `voice::choice::answer` in the desktop crate, which the live bridge reaches
 * through `desktop_voice_choice` and which re-checks the chosen entry against
 * what the app observes now.
 *
 * {@link answerChoiceLocally} is the same rule for a runtime with no Rust
 * behind it: the browser preview, and the vitest runtimes. It is the order and
 * the closed lists of the Rust function — cancel phrase, whole-utterance
 * ordinal, then a name among the OFFERED entries only — with the name matched
 * against labels rather than through each kind's resolver, and no liveness
 * check: the dispatch-time checks every target already makes are what stand
 * behind it there. The Rust function is the one production answers with.
 */
import type { VoiceResolvedParamDto } from "./bridge";

/**
 * The most candidates offered as a choice (`voice::choice::MAX_CHOICES`). A
 * longer tie keeps its sentence, which already summarises "and N more"; Rust
 * sends no candidates for one, and this is the webview's own guard.
 */
export const VOICE_CHOICE_MAX = 9;

/** What one utterance says about a pending choice (`voice::ChoiceAnswer`). */
export type VoiceChoiceAnswerDto =
  | { kind: "selected"; candidate: VoiceResolvedParamDto }
  | { kind: "refused" }
  | { kind: "not_answer" }
  | { kind: "cancelled" };

/* `voice::choice`'s closed lists, spelled the same. */
const CANCEL_PHRASES = ["cancel", "cancel that", "never mind", "nevermind", "none", "none of them", "neither", "no"];
const ORDINAL_LEADS = ["number", "option", "choice", "entry", "item"];
const CARDINALS = ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine"];
const ORDINALS = ["first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth"];
/* `WHOLE_UTTERANCE_POLITENESS` in `voice/outcome.rs`. */
const POLITENESS = ["okay", "ok", "alright", "yes", "yeah", "please", "just", "now", "thanks"];

/** `spoken_words`: lower case, every run of non-alphanumerics a break. */
function spokenWords(text: string): string[] {
  return text.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter((word) => word !== "");
}

/** `whole_utterance`: the words less politeness at either end. */
function wholeUtterance(text: string): string[] {
  const words = spokenWords(text);
  let start = 0;
  let end = words.length;
  while (start < end && POLITENESS.includes(words[start])) start += 1;
  while (end > start && POLITENESS.includes(words[end - 1])) end -= 1;
  return words.slice(start, end);
}

/** A whole-utterance ordinal, 1-based, `"last"`, or `undefined` for none. */
function ordinal(words: string[]): number | "last" | undefined {
  let rest = words;
  if (rest[0] === "the") rest = rest.slice(1);
  if (rest[0] !== undefined && ORDINAL_LEADS.includes(rest[0])) rest = rest.slice(1);
  if (rest.length === 2 && rest[1] === "one") rest = rest.slice(0, 1);
  if (rest.length !== 1) return undefined;
  const [word] = rest;
  if (word === "last") return "last";
  const named = CARDINALS.indexOf(word) >= 0 ? CARDINALS.indexOf(word) : ORDINALS.indexOf(word);
  if (named >= 0) return named + 1;
  const digits = word.replace(/(st|nd|rd|th)$/, "");
  return /^\d+$/.test(digits) ? Number(digits) : undefined;
}

/** Whether every word of `inner` is a word of `outer`. */
function subset(inner: string[], outer: string[]): boolean {
  return inner.length > 0 && inner.every((word) => outer.includes(word));
}

/**
 * Answer `utterance` against `offered` with no Rust behind the runtime. See
 * the module note for what this does and does not do.
 */
export function answerChoiceLocally(utterance: string, offered: readonly VoiceResolvedParamDto[]): VoiceChoiceAnswerDto {
  const words = wholeUtterance(utterance);
  if (words.length === 0) return { kind: "not_answer" };
  if (CANCEL_PHRASES.includes(words.join(" "))) return { kind: "cancelled" };
  const number = ordinal(words);
  if (number !== undefined) {
    const candidate = offered[number === "last" ? offered.length - 1 : number - 1];
    return candidate ? { kind: "selected", candidate } : { kind: "refused" };
  }
  const labels = offered.map((candidate) => spokenWords(candidate.label));
  const same = (label: string[]) => label.length === words.length && label.every((word, at) => word === words[at]);
  const exact = offered.filter((_, at) => same(labels[at]));
  const loose = exact.length > 0 ? exact : offered.filter((_, at) => subset(words, labels[at]) || subset(labels[at], words));
  if (loose.length === 1) return { kind: "selected", candidate: loose[0] };
  return loose.length > 1 ? { kind: "refused" } : { kind: "not_answer" };
}
