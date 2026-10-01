/**
 * PR #1451 round 3, change 3 (PRD #1261) — numbers on lists while voice is on.
 *
 * While voice is on, each list voice selects from shows a number beside every
 * item: the dashboard's agents, the Daemons screen's tiles, and the New agent
 * dialog's daemons, directories and modes. A bare number said aloud ("three",
 * "number three", "the third one") selects the item showing it, answered
 * LOCALLY — no Commands backend call — by `voice::numbers::answer` in the
 * desktop crate, which the live bridge reaches through `desktop_voice_number`.
 *
 * # The declaration
 *
 * {@link VoiceNumberedListDto} is what is numbered on screen: the items in
 * reading order, entry `i` showing number `i + 1`, and a `generation` that
 * changes whenever the items do. One screen at a time declares it — the New
 * agent dialog while it is open (its three lists numbered continuously), else
 * the dashboard or the Daemons screen — and nothing behind an open pane or
 * dialog is numbered. A listing that pages declares its CURRENT page only, so
 * its numbers restart at 1 on every page and a page turn is a new generation.
 *
 * The voice panel keeps the declaration as it stood when the user began to
 * speak and sends it with the utterance, beside the generation on screen
 * when the answer is worked out: a list that changed in between is refused,
 * since the number may now be another item's.
 *
 * {@link answerNumberLocally} is the same rule for a runtime with no Rust
 * behind it — the browser preview and the vitest runtimes — built on
 * `voiceChoice.ts`'s port of the ordinal reading. The Rust function is the one
 * production answers with.
 */
import type { VoiceOutcomeDto, VoiceResolvedParamDto } from "./bridge";
import { CARDINALS, ordinal, ordinalWord, spokenWords, wholeUtterance } from "./voiceChoice";

/** What a numbered item is, which decides what choosing it does (`voice::numbers::NumberedKind`). */
export type VoiceNumberedKind = "agent" | "deck" | "directory" | "parent" | "mode";

/** One numbered item as the screen shows it (`voice::numbers::VoiceNumberedEntry`). */
export interface VoiceNumberedEntryDto {
  kind: VoiceNumberedKind;
  /** Its identity on that list: an agent id, a deck id, a path, a mode id. */
  value: string;
  /** For an agent, the deck it is on: the dashboard spans every deck. */
  deckId?: string;
  /** The name shown beside the number. */
  label: string;
  /** Every other name the item answers to, which a spoken number can collide with. */
  names: string[];
}

/** The numbered list on screen (`voice::numbers::VoiceNumberedList`). */
export interface VoiceNumberedListDto {
  generation: number;
  entries: VoiceNumberedEntryDto[];
}

/** What one utterance says about the numbered list (`voice::numbers::NumberAnswer`). Numbers are 1-based. */
export type VoiceNumberAnswerDto =
  | { kind: "not_number" }
  | { kind: "selected"; number: number }
  | { kind: "out_of_range"; number: number }
  | { kind: "stale" }
  | { kind: "ambiguous"; numbers: number[] };

/** No numbered list: what a screen with nothing numbered declares. */
export const NO_NUMBERED_LIST: VoiceNumberedListDto = { generation: 0, entries: [] };

/** `voice::numbers::is_count`: a cardinal or plain digits, never a position. */
function isCount(word: string): boolean {
  return CARDINALS.includes(word) || /^\d+$/.test(word);
}

/** `voice::numbers::ends_in`: one of the entry's names is, or ends in, `number`. */
function endsIn(entry: VoiceNumberedEntryDto, number: number): boolean {
  return [entry.label, ...entry.names].some((name) => {
    const last = spokenWords(name).at(-1);
    return last !== undefined && isCount(last) && ordinal([last]) === number;
  });
}

/**
 * Answer `utterance` against `heard` — the list as it stood when the user
 * spoke — given the generation on screen `now`. `voice::numbers::answer`,
 * rule for rule; see its documentation for the order.
 */
export function answerNumberLocally(utterance: string, heard: VoiceNumberedListDto, now: number): VoiceNumberAnswerDto {
  const words = wholeUtterance(utterance);
  const said = ordinal(words);
  if (said === undefined || heard.entries.length === 0) return { kind: "not_number" };
  if (heard.generation !== now) return { kind: "stale" };
  const number = said === "last" ? heard.entries.length : said;
  if (number < 1 || number > heard.entries.length) return { kind: "out_of_range", number: said === "last" ? 0 : said };
  const word = ordinalWord(words);
  const counted = said !== "last" && word !== undefined && isCount(word);
  const numbers = [number];
  if (counted) heard.entries.forEach((entry, at) => { if (at !== number - 1 && endsIn(entry, number)) numbers.push(at + 1); });
  return numbers.length > 1 ? { kind: "ambiguous", numbers } : { kind: "selected", number };
}

/** Each kind's row: the command a spoken name for that item would have run, and its param. */
const ROWS: Record<VoiceNumberedKind, { action: string; invoke: string; param?: { name: string; kind: string }; report: (label: string) => string }> = {
  agent: { action: "open_agent", invoke: "openAgent", param: { name: "agent", kind: "agent_ref" }, report: (label) => `Opening ${label}.` },
  deck: { action: "choose_deck", invoke: "chooseNewAgentDeck", param: { name: "deck", kind: "deck_ref" }, report: (label) => `Daemon: ${label}.` },
  directory: { action: "open_dir", invoke: "openDirectory", param: { name: "dir", kind: "dir_ref" }, report: (label) => `Opening ${label}.` },
  parent: { action: "go_to_parent", invoke: "goToParentDirectory", report: () => "Going up." },
  mode: { action: "choose_mode", invoke: "chooseNewAgentMode", param: { name: "mode", kind: "mode_ref" }, report: (label) => `Mode: ${label}.` },
};

/**
 * The item as a resolved param of its row's kind — what the dispatch reads,
 * and what a numbered choice lists. `value` is the item's identity; an agent
 * carries its deck, since the dashboard's rows span every deck.
 */
export function numberedParam(entry: VoiceNumberedEntryDto, spoken: string): VoiceResolvedParamDto {
  const param = ROWS[entry.kind].param ?? { name: "dir", kind: "dir_ref" };
  return {
    name: param.name,
    kind: param.kind,
    spoken,
    value: entry.value,
    label: entry.label,
    ...(entry.deckId !== undefined ? { deckId: entry.deckId } : {}),
    ...(entry.names.length > 0 ? { names: entry.names } : {}),
  };
}

/**
 * Choosing `entry` as a dispatch: the row a spoken name for it would have run,
 * with the item as its param, reported as that row reports.
 */
export function numberedOutcome(entry: VoiceNumberedEntryDto, transcript: string): Extract<VoiceOutcomeDto, { kind: "dispatch" }> {
  const row = ROWS[entry.kind];
  return {
    kind: "dispatch",
    transcript,
    action: row.action,
    invoke: row.invoke,
    params: row.param ? [numberedParam(entry, transcript)] : [],
    sentence: row.report(entry.label),
  };
}

/** Whether two declarations number the same items, in the same order, under the same names. */
export function sameNumberedEntries(left: readonly VoiceNumberedEntryDto[], right: readonly VoiceNumberedEntryDto[]): boolean {
  return left.length === right.length && left.every((entry, at) => {
    const other = right[at];
    return entry.kind === other.kind && entry.value === other.value && entry.deckId === other.deckId && entry.label === other.label
      && entry.names.length === other.names.length && entry.names.every((name, index) => name === other.names[index]);
  });
}

/**
 * The number a key press means for a numbered list: a plain digit 1–9 with no
 * modifier, not typed into a field or a terminal, where a digit is text. A
 * list that takes it selects the item showing that number.
 */
export function numberKey(event: KeyboardEvent): number | undefined {
  if (event.ctrlKey || event.metaKey || event.altKey || !/^[1-9]$/.test(event.key)) return undefined;
  const target = event.target;
  if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea")) return undefined;
  return Number(event.key);
}
