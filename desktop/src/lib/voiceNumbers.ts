/**
 * PR #1451 round 3, change 3 (PRD #1261) — numbers on lists while voice is on.
 *
 * While voice is on, each list voice selects from shows a number beside every
 * item: the dashboard's agents, the Daemons screen's tiles, and the New agent
 * dialog's daemons, directories and modes. Each SECTION — one kind of list —
 * is numbered from 1 (round 4, D7). A number said aloud, bare ("three",
 * "number three", "the third one") or with its section ("directory 13",
 * "select daemon 1", "choose mode 2", "open agent 3"), selects the item
 * showing it, answered LOCALLY — no Commands backend call — by
 * `voice::numbers::answer` in the desktop crate, which the live bridge reaches
 * through `desktop_voice_number`. A bare number shown by several sections is
 * offered as the numbered choice between them.
 *
 * # The declaration
 *
 * {@link VoiceNumberedListDto} is what is numbered on screen: its sections in
 * reading order, entry `i` of a section showing number `i + 1`, and a
 * `generation` that changes whenever the items do. One screen at a time
 * declares it — the New agent dialog while it is open (daemons, directories
 * and modes, each from 1), else the dashboard or the Daemons screen — and
 * nothing behind an open pane or dialog is numbered. A listing that pages
 * declares its CURRENT page only, so its numbers restart at 1 on every page
 * and a page turn is a new generation.
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
import { isCountWord, ordinal, saidAsCount, spokenWords, wholeUtterance } from "./voiceChoice";

/** What a numbered item is, which decides what choosing it does (`voice::numbers::NumberedKind`). */
export type VoiceNumberedKind = "agent" | "deck" | "deck_switch" | "directory" | "parent" | "mode";

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

/** One kind of list on screen, numbered from 1 (`voice::numbers::SectionKind`). */
export type VoiceNumberedSectionKind = "agent" | "deck" | "directory" | "mode";

/** One section of the numbered list: entry `i` shows number `i + 1` (`voice::numbers::VoiceNumberedSection`). */
export interface VoiceNumberedSectionDto {
  kind: VoiceNumberedSectionKind;
  entries: VoiceNumberedEntryDto[];
}

/** The numbered list on screen (`voice::numbers::VoiceNumberedList`). */
export interface VoiceNumberedListDto {
  generation: number;
  sections: VoiceNumberedSectionDto[];
}

/** One numbered item, by its section and the number it shows (`voice::numbers::NumberRef`). */
export interface VoiceNumberRefDto {
  section: VoiceNumberedSectionKind;
  number: number;
}

/** What one utterance says about the numbered list (`voice::numbers::NumberAnswer`). Numbers are 1-based. */
export type VoiceNumberAnswerDto =
  | { kind: "not_number" }
  /* `section` is always sent by Rust; an answer without one (the shape before
     round 4) means the first section showing that number. */
  | { kind: "selected"; section?: VoiceNumberedSectionKind; number: number }
  | { kind: "out_of_range"; section?: VoiceNumberedSectionKind; number: number; elsewhere: VoiceNumberedSectionKind[] }
  | { kind: "not_numbered"; section: VoiceNumberedSectionKind; shown: VoiceNumberedSectionKind[] }
  | { kind: "stale" }
  | { kind: "ambiguous"; choices: VoiceNumberRefDto[] };

/** No numbered list: what a screen with nothing numbered declares. */
export const NO_NUMBERED_LIST: VoiceNumberedListDto = { generation: 0, sections: [] };

/** How a section is named in a sentence, singular and plural, and capitalised for a choice's entry. */
export const SECTION_NOUNS: Record<VoiceNumberedSectionKind, { one: string; a: string; many: string; title: string }> = {
  agent: { one: "agent", a: "an agent", many: "agents", title: "Agent" },
  deck: { one: "daemon", a: "a daemon", many: "daemons", title: "Daemon" },
  directory: { one: "directory", a: "a directory", many: "directories", title: "Directory" },
  mode: { one: "mode", a: "a mode", many: "modes", title: "Mode" },
};

/** `voice::numbers::SectionKind::words`: the words that name each section. */
const SECTION_WORDS: Record<VoiceNumberedSectionKind, readonly string[]> = {
  agent: ["agent", "agents"],
  deck: ["daemon", "daemons", "deck", "decks"],
  directory: ["directory", "directories", "folder", "folders", "dir", "dirs"],
  mode: ["mode", "modes"],
};

/** `voice::numbers::LEADING_VERBS`: verbs that may lead a section word. */
const LEADING_VERBS: readonly (readonly string[])[] = [["select"], ["choose"], ["pick"], ["open"], ["enter"], ["go", "to"], ["switch", "to"]];

/** The item showing `number` in the declared section `section` — with no section, the first section showing it — if any. */
export function numberedEntry(list: VoiceNumberedListDto, section: VoiceNumberedSectionKind | undefined, number: number): VoiceNumberedEntryDto | undefined {
  const sections = shownSections(list);
  return section === undefined
    ? sections.find((candidate) => candidate.entries[number - 1] !== undefined)?.entries[number - 1]
    : sections.find((candidate) => candidate.kind === section)?.entries[number - 1];
}

/** Whether anything is numbered. */
export function hasNumbered(list: VoiceNumberedListDto): boolean {
  return list.sections.some((section) => section.entries.length > 0);
}

/** `VoiceNumberedList::shown`: the sections that number something, the first of each kind. */
function shownSections(list: VoiceNumberedListDto): VoiceNumberedSectionDto[] {
  const shown: VoiceNumberedSectionDto[] = [];
  for (const section of list.sections) {
    if (section.entries.length > 0 && !shown.some((seen) => seen.kind === section.kind)) shown.push(section);
  }
  return shown;
}

/** `voice::numbers::read`: the section said, if any — with the word that said it — and the words that should be the number. */
function readSection(words: string[]): { section?: VoiceNumberedSectionKind; word?: string; said: string[] } | undefined {
  const verb = LEADING_VERBS.find((lead) => words.length >= lead.length && lead.every((word, at) => words[at] === word))?.length ?? 0;
  let rest = words.slice(verb);
  if (rest[0] === "the") rest = rest.slice(1);
  const section = (Object.keys(SECTION_WORDS) as VoiceNumberedSectionKind[]).find((kind) => rest[0] !== undefined && SECTION_WORDS[kind].includes(rest[0]));
  if (section !== undefined) return { section, word: rest[0], said: rest.slice(1) };
  return verb === 0 ? { said: words } : undefined;
}

/** `voice::numbers::ends_in`: one of the entry's names is, or ends in, `number`. */
function endsIn(entry: VoiceNumberedEntryDto, number: number): boolean {
  return [entry.label, ...entry.names].some((name) => {
    const last = spokenWords(name).at(-1);
    return last !== undefined && isCountWord(last) && ordinal([last]) === number;
  });
}

/** `voice::choice::Ordinal::index`: the 0-based index `said` names in `len` entries, if any. */
function indexOf(said: number | "last", len: number): number | undefined {
  const at = said === "last" ? len - 1 : said - 1;
  return at >= 0 && at < len ? at : undefined;
}

/** `voice::numbers::is_named`: one of the entry's names is `word` followed by `number` as a count. */
function isNamed(entry: VoiceNumberedEntryDto, word: string, number: number): boolean {
  return [entry.label, ...entry.names].some((name) => {
    const words = spokenWords(name);
    return words.length === 2 && words[0] === word && isCountWord(words[1]) && ordinal([words[1]]) === number;
  });
}

/**
 * `voice::numbers::Collides`: nothing for a position, every name ending in
 * the number for a bare count, and a name that IS the section word said and
 * the number after one ("folder 13" and `folder-13`).
 */
type Collides = { kind: "nothing" } | { kind: "ends_in" } | { kind: "named"; word: string };

/** `voice::numbers::named`: the items of `section` the number at `at` names. */
function named(section: VoiceNumberedSectionDto, at: number, collides: Collides): VoiceNumberRefDto[] {
  const items = [{ section: section.kind, number: at + 1 }];
  const collidesWith = (entry: VoiceNumberedEntryDto) => (collides.kind === "ends_in" ? endsIn(entry, at + 1) : collides.kind === "named" && isNamed(entry, collides.word, at + 1));
  section.entries.forEach((entry, other) => { if (other !== at && collidesWith(entry)) items.push({ section: section.kind, number: other + 1 }); });
  return items;
}

/**
 * Answer `utterance` against `heard` — the list as it stood when the user
 * spoke — given the generation on screen `now`. `voice::numbers::answer`,
 * rule for rule; see its documentation for the order.
 */
export function answerNumberLocally(utterance: string, heard: VoiceNumberedListDto, now: number): VoiceNumberAnswerDto {
  const read = readSection(wholeUtterance(utterance));
  const said = read === undefined ? undefined : ordinal(read.said);
  const shown = shownSections(heard);
  if (read === undefined || said === undefined || shown.length === 0) return { kind: "not_number" };
  if (heard.generation !== now) return { kind: "stale" };
  const counted = said !== "last" && saidAsCount(read.said);
  const collides: Collides = !counted ? { kind: "nothing" } : read.word === undefined ? { kind: "ends_in" } : { kind: "named", word: read.word };
  const number = said === "last" ? 0 : said;
  let choices: VoiceNumberRefDto[];
  if (read.section !== undefined) {
    const kind = read.section;
    const listed = shown.find((section) => section.kind === kind);
    if (listed === undefined) return { kind: "not_numbered", section: kind, shown: shown.map((section) => section.kind) };
    const at = indexOf(said, listed.entries.length);
    if (at === undefined) {
      const elsewhere = said === "last" ? [] : shown.filter((other) => other.kind !== kind && indexOf(said, other.entries.length) !== undefined).map((other) => other.kind);
      return { kind: "out_of_range", section: kind, number, elsewhere };
    }
    choices = named(listed, at, collides);
  } else {
    choices = shown.flatMap((listed) => {
      const at = indexOf(said, listed.entries.length);
      return at === undefined ? [] : named(listed, at, collides);
    });
  }
  if (choices.length === 0) return { kind: "out_of_range", number, elsewhere: [] };
  if (choices.length === 1) return { kind: "selected", section: choices[0].section, number: choices[0].number };
  return { kind: "ambiguous", choices };
}

/** Each kind's row: the command a spoken name for that item would have run, and its param. */
const ROWS: Record<VoiceNumberedKind, { action: string; invoke: string; param?: { name: string; kind: string }; report: (label: string) => string }> = {
  agent: { action: "open_agent", invoke: "openAgent", param: { name: "agent", kind: "agent_ref" }, report: (label) => `Opening ${label}.` },
  deck: { action: "choose_deck", invoke: "chooseNewAgentDeck", param: { name: "deck", kind: "deck_ref" }, report: (label) => `Daemon: ${label}.` },
  /* The Daemon selector's menu, numbered while it is open (PR #1451 round 3, change 4). */
  deck_switch: { action: "switch_deck", invoke: "switchDeck", param: { name: "deck", kind: "deck_ref" }, report: (label) => `Showing ${label}.` },
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

/** Whether two sections' entries number the same items, in the same order, under the same names. */
function sameNumberedEntries(left: readonly VoiceNumberedEntryDto[], right: readonly VoiceNumberedEntryDto[]): boolean {
  return left.length === right.length && left.every((entry, at) => {
    const other = right[at];
    return entry.kind === other.kind && entry.value === other.value && entry.deckId === other.deckId && entry.label === other.label
      && entry.names.length === other.names.length && entry.names.every((name, index) => name === other.names[index]);
  });
}

/** Whether two declarations number the same items, section by section, in the same order, under the same names. */
export function sameNumberedSections(left: readonly VoiceNumberedSectionDto[], right: readonly VoiceNumberedSectionDto[]): boolean {
  return left.length === right.length && left.every((section, at) => section.kind === right[at].kind && sameNumberedEntries(section.entries, right[at].entries));
}

/**
 * The number a key press means for a numbered list: a plain digit 1–9 with no
 * modifier, not typed into a field or a terminal, where a digit is text. A
 * list that takes it selects the item showing that number in ITS OWN section
 * — the one with keyboard focus (round 4, D7).
 */
export function numberKey(event: KeyboardEvent): number | undefined {
  if (event.ctrlKey || event.metaKey || event.altKey || !/^[1-9]$/.test(event.key)) return undefined;
  const target = event.target;
  if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea")) return undefined;
  return Number(event.key);
}
