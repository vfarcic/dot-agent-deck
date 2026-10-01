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
 * the closed lists of the Rust function — a bare control that is also an
 * offered label (refused), cancel phrase, whole-utterance ordinal, then a name
 * among the OFFERED entries only, said on its own (PR #1451 review: "stop
 * Planner" is a new command, not an answer) — with the name matched against
 * each entry's label and the `names` Rust supplied with it (the per-kind list
 * `voice::choice::answer` itself matches against, `ResolvedParam::names`)
 * rather than through each kind's resolver, never against `value`, and no
 * liveness check: the dispatch-time checks every target already makes are
 * what stand behind it there. The Rust function is the one production
 * answers with.
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

/*
 * `voice::choice::ANSWER_FILLER`: the words an answer may carry around a name
 * without naming anything else — an article, and the noun for what is being
 * chosen, by param kind.
 */
const ARTICLES = ["the", "a", "an"];
const KIND_NOUNS: Record<string, string[]> = {
  agent_ref: ["agent"],
  deck_ref: ["deck", "daemon"],
  dir_ref: ["folder", "directory"],
  orchestration_ref: ["orchestration", "run"],
  mode_ref: ["mode", "chip"],
  agent_type_ref: ["agent", "type"],
};

/**
 * `voice::choice::covers`: whether the answer `words` say nothing but `label`
 * — every word is a word of the label or filler, and at least one is the
 * label's. A command around a name ("stop Planner") or a name with extra words
 * does not.
 */
function covers(words: string[], label: string[], kind: string): boolean {
  const filler = [...ARTICLES, ...(KIND_NOUNS[kind] ?? [])];
  return words.some((word) => label.includes(word)) && words.every((word) => label.includes(word) || filler.includes(word));
}

/**
 * The 1-based number of the offered entry whose label, or another of its
 * spoken names, is, word for word, a bare ordinal or cancel phrase that
 * `utterance` also is — an agent named "two", or whose role is "cancel" — or `undefined` when there is no such collision. Such an
 * utterance is refused rather than read either way, here and in
 * `voice::choice::answer`; the panel uses the number to say "number N".
 */
export function collidingChoiceEntry(utterance: string, offered: readonly VoiceResolvedParamDto[]): number | undefined {
  const words = wholeUtterance(utterance);
  if (words.length === 0) return undefined;
  if (!CANCEL_PHRASES.includes(words.join(" ")) && ordinal(words) === undefined) return undefined;
  const said = words.join(" ");
  /* The label and every other name the entry answers to (`names`, from
     `voice::choice::names_of`): an agent labelled "Builder" whose role is
     "two" collides as surely as one labelled "two" (Qodo on PR #1451). */
  const at = offered.findIndex((candidate) => [candidate.label, ...(candidate.names ?? [])].some((name) => spokenWords(name).join(" ") === said));
  return at >= 0 ? at + 1 : undefined;
}

/**
 * Answer `utterance` against `offered` with no Rust behind the runtime. See
 * the module note for what this does and does not do.
 */
export function answerChoiceLocally(utterance: string, offered: readonly VoiceResolvedParamDto[]): VoiceChoiceAnswerDto {
  const words = wholeUtterance(utterance);
  if (words.length === 0) return { kind: "not_answer" };
  if (collidingChoiceEntry(utterance, offered) !== undefined) return { kind: "refused" };
  if (CANCEL_PHRASES.includes(words.join(" "))) return { kind: "cancelled" };
  const number = ordinal(words);
  if (number !== undefined) {
    const candidate = offered[number === "last" ? offered.length - 1 : number - 1];
    return candidate ? { kind: "selected", candidate } : { kind: "refused" };
  }
  /* Each entry's label and the names Rust supplied with it — never `value`,
     which is an ID the user may not know and was not shown. */
  const names = offered.map((candidate) => [candidate.label, ...(candidate.names ?? [])].map(spokenWords));
  const same = (name: string[]) => name.length === words.length && name.every((word, at) => word === words[at]);
  const exact = offered.filter((_, at) => names[at].some(same));
  const loose = exact.length > 0 ? exact : offered.filter((candidate, at) => names[at].some((name) => covers(words, name, candidate.kind)));
  if (loose.length === 1) return { kind: "selected", candidate: loose[0] };
  return loose.length > 1 ? { kind: "refused" } : { kind: "not_answer" };
}
