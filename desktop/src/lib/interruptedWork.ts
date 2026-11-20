/**
 * PRD #1790 stage 2 — the desktop's interrupted work: the work a deck
 * restart left behind, as each deck's daemon derives it from its work journal.
 *
 * The constants in the "Decision D2" section below are copies of the ones in
 * `src/interrupted_work.rs` (decision D2), so the CLI, the TUI and the desktop
 * say the same thing (CLAUDE.md rule 22); `interruptedWork.test.ts` reads that
 * file and fails when one of them drifts, and checks several helpers'
 * sentences against the Rust helpers'. The desktop-only sentences above that
 * section — the spoken dismissal's question and what voice is told when it is
 * refused — are the desktop's own, not copies.
 *
 * The types mirror the daemon's own answers (`InterruptedList`,
 * `InterruptedShow`, `ReclaimOutcome`, `DismissOutcome`), which the app's
 * backend passes through as the daemon sent them, snake_case and all.
 */

import type { VoiceResolvedParamDto } from "./bridge";
import { collidingChoiceEntry, ordinal, spokenWords } from "./voiceChoice";

// ---------------------------------------------------------------------------
// The daemon's answers.
// ---------------------------------------------------------------------------

export type InterruptedItemKind = "unit" | "orchestration" | "issue_dispatch" | "agent";

export interface InterruptedLastEntry { kind: string; ts_ms: number }

export interface InterruptedItemReport {
  report_id: string;
  task_id?: string;
  unit_id?: string;
  kept_until_ms?: number | null;
}

export interface InterruptedProgress {
  attempts: number;
  roles?: string[];
  delegations: number;
  delegations_done: number;
}

export type PrFact = "open" | "merged" | "closed" | "none" | "unknown";

export interface InterruptedGitFacts {
  worktree_exists?: boolean | null;
  dirty?: boolean | null;
  unpushed_commits?: number | null;
  branch_exists?: boolean | null;
  pr?: PrFact;
}

export interface InterruptedItem {
  id: string;
  kind: InterruptedItemKind;
  name?: string | null;
  issue?: number | null;
  repo?: string | null;
  branch?: string | null;
  worktree?: string | null;
  launcher?: string | null;
  last_entry: InterruptedLastEntry;
  reports: InterruptedItemReport[];
  reports_omitted?: number;
  cut?: boolean;
  progress: InterruptedProgress;
  git?: InterruptedGitFacts | null;
}

export interface InterruptedList {
  items: InterruptedItem[];
  incomplete?: boolean;
  omitted?: number;
}

export interface ShownReport {
  report_id: string;
  body?: string | null;
  cut?: boolean;
  error?: string | null;
}

export interface InterruptedShow {
  item: InterruptedItem;
  reports: ShownReport[];
  incomplete?: boolean;
  bodies_omitted?: number;
  reports_from?: number;
  next_reports_from?: number | null;
}

export type RefusalReason =
  | "agent-caller" | "bad-request" | "too-large" | "unreadable" | "incomplete-read" | "not-listed" | "ambiguous"
  | "plain-agent" | "no-worktree" | "not-linked-worktree" | "branch-mismatch" | "not-owned" | "still-in-use"
  | "in-use-unknown" | "not-recorded" | "unknown";

export interface Refusal { reason: RefusalReason | string; message: string }

export type ReclaimVerdict =
  | "removed" | "would-remove" | "kept-dirty" | "kept-holds-worktrees" | "kept-could-not-check" | "remove-failed"
  | "timed-out" | "kept-changed" | "kept-busy" | "kept-could-not-lock" | "refused" | "unknown";

export interface ReclaimOutcome {
  id?: string | null;
  dry_run?: boolean;
  verdict: ReclaimVerdict | string;
  message: string;
  refusal?: Refusal | null;
  worktree?: string | null;
  branch?: string | null;
  unpushed_commits?: number | null;
  nested_worktrees?: string[];
  recorded?: boolean;
}

/** `DismissStopped`: why a dismissal sent in batches stopped, and how many of its ids it did not reach. */
export interface DismissStopped {
  reason: "replaced" | "refused" | "failed" | string;
  refusal?: string | null;
  not_sent: number;
}

export interface DismissOutcome {
  dismissed: { id: string; name?: string | null }[];
  refused: { id: string; refusal: Refusal }[];
  refusal?: Refusal | null;
  incomplete?: boolean;
  dismissed_omitted?: number;
  refused_omitted?: number;
  /** Set by the app's bridge when a later batch of a dismissal sent in batches stopped the rest (`DismissOutcome::stopped`). */
  stopped?: DismissStopped | null;
}

/**
 * One answer of a deck's interrupted work verbs (`InterruptedWorkReply` in
 * the desktop crate): the daemon's answer and the process it came from, or
 * `unsupported` — the deck's daemon does not advertise `interrupted-work`, or
 * is no longer the process asked of, and nothing was sent. Shown as nothing.
 */
export type InterruptedReply<T> = { outcome: "answered"; instance: string; answer: T } | { outcome: "unsupported" };

/**
 * Read an interrupted work answer from the app's backend: `{ outcome:
 * "answered", instance, answer }` whose `answer` `isAnswer` accepts, or
 * anything else as `unsupported`, so a malformed or unknown reply shows
 * nothing rather than breaking the dashboard.
 */
export function normalizeInterruptedReply<T>(value: unknown, isAnswer: (answer: unknown) => answer is T): InterruptedReply<T> {
  if (typeof value !== "object" || value === null) return { outcome: "unsupported" };
  const record = value as Record<string, unknown>;
  if (record.outcome !== "answered" || typeof record.instance !== "string" || !isAnswer(record.answer)) return { outcome: "unsupported" };
  return { outcome: "answered", instance: record.instance, answer: record.answer };
}

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;
/** A list answer: an object with an `items` array. */
export const isInterruptedList = (value: unknown): value is InterruptedList => isRecord(value) && Array.isArray(value.items);
/** A show answer: an `item` and a `reports` array. */
export const isInterruptedShow = (value: unknown): value is InterruptedShow => isRecord(value) && isRecord(value.item) && Array.isArray(value.reports);
/** A reclaim answer: a `verdict` and a `message`. */
export const isReclaimOutcome = (value: unknown): value is ReclaimOutcome => isRecord(value) && typeof value.verdict === "string" && typeof value.message === "string";
/** A dismissal answer: `dismissed` and `refused` arrays. */
export const isDismissOutcome = (value: unknown): value is DismissOutcome => isRecord(value) && Array.isArray(value.dismissed) && Array.isArray(value.refused);

/** The deck and daemon process a request is for: an item's id belongs to one daemon's state directory. */
export interface InterruptedTarget { deckId: string; instance: string }

/**
 * What the dialog serves to its buttons, its keys and voice — the same
 * calls by every route. Each that can be refused answers the sentence why;
 * reclaim and dismiss answer, once settled, the sentence for their outcome.
 *
 * `item` is what the user said for which item (an ordinal, or its name,
 * branch or id; see {@link resolveSpokenItem}); without it, the selected row.
 */
export interface InterruptedDialogControls {
  /** Run the dry run of a reclaim; the reclaim itself waits for {@link confirmReclaim}. */
  reclaim: (item?: string) => string | Promise<string | undefined>;
  confirmReclaim: () => string | undefined;
  /** Dismiss the selected item now: the Dismiss button's and `d`'s, as the TUI's `d`. */
  dismiss: () => string | Promise<string | undefined>;
  /** Ask whether to dismiss the item `item` names, bound to its id: nothing is sent until {@link confirmDismiss}. */
  askDismiss: (item?: string) => string | undefined;
  confirmDismiss: () => string | undefined;
  dismissAll: () => string | undefined;
  confirmDismissAll: () => string | undefined;
  select: (id: string) => void;
  /** Open the details of the item `id` names, or of the one `item` names. */
  openDetails: (which?: { item?: string; id?: string }) => string | undefined;
  /** Show the first page of an item's kept reports: the one `item` names, the open details' item, or the selected one. */
  showReports: (item?: string) => string | undefined;
  nextReports: () => string | undefined;
  previousReports: () => string | undefined;
  back: () => void;
  close: () => void;
}

/** Desktop-only: the question a spoken Dismiss of one item asks before anything is sent. */
export function dismissOneQuestion(item: InterruptedItem): string {
  return `${DISMISS_ACTION} ${itemName(item)} · ${kindLabel(item.kind)}? [y/N]`;
}

/** What an item answers to when the user names it: its name and its branch, as words. */
function spokenNames(item: InterruptedItem): string[][] {
  return [item.name, item.branch].filter((value): value is string => typeof value === "string" && value !== "").map(spokenWords);
}

/** Whether `words` occur in `name`, in order and next to each other. */
function containsRun(name: readonly string[], words: readonly string[]): boolean {
  if (words.length === 0 || words.length > name.length) return false;
  for (let at = 0; at + words.length <= name.length; at += 1) {
    if (words.every((word, offset) => name[at + offset] === word)) return true;
  }
  return false;
}

/**
 * PRD #1790 stage 2 (reviewer 10, auditor 3) — the item `said` names in
 * `ordered`, or the sentence why none: no words are the `selected` row; an
 * ordinal ("the second one", "last") is that position, unless the same words
 * are also another item's whole name (`voiceChoice`'s collision rule), which
 * is refused as ambiguous; otherwise a name, branch or id. Names and branches
 * are compared as runs of words, so "fix login" names `fix-login`; an exact
 * match wins over a partial one, and two of either kind are refused.
 */
export function resolveSpokenItem(ordered: readonly InterruptedItem[], selected: InterruptedItem | undefined, said?: string): InterruptedItem | string {
  if (said === undefined || said.trim() === "") return selected ?? REFUSED_NOT_LISTED;
  const words = spokenWords(said);
  if (words.length === 0) return REFUSED_NOT_LISTED;
  const position = ordinal(words);
  if (position !== undefined) {
    const at = position === "last" ? ordered.at(-1) : ordered[position - 1];
    const offered = ordered.map((item): VoiceResolvedParamDto => ({ name: "item", kind: "filter_text", spoken: item.name ?? item.id, value: item.id, label: item.name ?? item.branch ?? item.id, names: item.branch ? [item.branch] : [] }));
    const colliding = collidingChoiceEntry(said, offered);
    if (colliding !== undefined && ordered[colliding - 1]?.id !== at?.id) return REFUSED_AMBIGUOUS;
    return at ?? REFUSED_NOT_LISTED;
  }
  const phrase = words.join(" ");
  const raw = said.trim().toLowerCase();
  const exact = ordered.filter((item) => item.id.toLowerCase() === raw || spokenNames(item).some((name) => name.join(" ") === phrase));
  if (exact.length === 1) return exact[0];
  if (exact.length > 1) return REFUSED_AMBIGUOUS;
  const partial = ordered.filter((item) => [...spokenNames(item), spokenWords(item.id)].some((name) => containsRun(name, words)));
  if (partial.length === 1) return partial[0];
  return partial.length > 1 ? REFUSED_AMBIGUOUS : REFUSED_NOT_LISTED;
}

/**
 * PRD #1790 stage 2 (auditor 2) — the open dialog as voice sees it: `token`
 * names its deck, daemon process, opening, listed items, selection and view,
 * and `covered` says whether another modal is drawn over it. The voice
 * surface reads the token when an utterance is declared and refuses an
 * answer for the dialog once it moved; the dashboard refuses a spoken action
 * while the dialog is covered.
 */
export interface ShownInterruptedDialog { token: string; covered: () => boolean }
let shownDialog: ShownInterruptedDialog | undefined;

/** The dialog on screen, or `undefined` with none. */
export function shownInterruptedDialog(): ShownInterruptedDialog | undefined {
  return shownDialog;
}

/** The token of the dialog on screen, or `undefined` with none (read when an utterance is declared). */
export function shownInterruptedToken(): string | undefined {
  return shownDialog?.token;
}

/** Publish the dialog on screen; answers the unpublish, which only takes back its own. */
export function publishInterruptedDialog(dialog: ShownInterruptedDialog): () => void {
  shownDialog = dialog;
  return () => {
    if (shownDialog === dialog) shownDialog = undefined;
  };
}

/** What a spoken action on the dialog says while another modal covers it. */
export const INTERRUPTED_COVERED = "Something is open over the interrupted work; close it first.";
/** …while a reclaim or dismissal of this daemon is still waiting for its answer. */
export const INTERRUPTED_WAIT = "Wait for the last action to finish.";
/** …asked for an item's reports when it keeps none. */
export const INTERRUPTED_NO_REPORTS = "That item has no kept reports.";
/** …asked for another page of reports when there is none that way. */
export const INTERRUPTED_NO_NEXT_PAGE = "There is no next page of reports.";
export const INTERRUPTED_NO_PREVIOUS_PAGE = "There is no previous page of reports.";

// ---------------------------------------------------------------------------
// Decision D2: the words (copies of `src/interrupted_work.rs`).
// ---------------------------------------------------------------------------

export const INTERRUPTED_WORK_TITLE = "Interrupted work";
export const OTHER_AGENTS_GROUP = "Other agents";
export const RECLAIM_ACTION = "Reclaim";
export const RECLAIM_EXPLANATION = "remove its worktree, keep its branch";
export const DISMISS_ACTION = "Dismiss";
export const DISMISS_ALL_ACTION = "Dismiss all";
export const KEPT_UNTIL = "kept until";

export const RECLAIM_REMOVED = "Reclaimed: its worktree was removed and its branch kept.";
export const RECLAIM_WOULD_REMOVE = "Reclaim would remove its worktree and keep its branch.";
export const RECLAIM_KEPT_DIRTY = "Kept: its worktree has uncommitted changes. Commit or discard them, then reclaim it again.";
export const RECLAIM_KEPT_HOLDS_WORKTREES = "Kept: other worktrees lie inside its worktree. Remove them first, then reclaim it again.";
export const RECLAIM_KEPT_COULD_NOT_CHECK = "Kept: whether its worktree has uncommitted changes could not be checked.";
export const RECLAIM_REMOVE_FAILED = "Kept: git could not remove its worktree.";
export const RECLAIM_TIMED_OUT = "Removing its worktree did not finish in time; it may still be removed, and the deck records it if it is.";
export const RECLAIM_KEPT_CHANGED = "Kept: its worktree was moved, replaced or switched to another branch while it was being reclaimed, so nothing was removed. List the interrupted work again.";
export const RECLAIM_KEPT_BUSY = "Kept: the deck could not get its repository's worktree lock in time, so nothing was removed. Reclaim it again.";
export const RECLAIM_KEPT_COULD_NOT_LOCK = "Kept: the deck could not take its repository's worktree lock, so nothing was removed.";
export const RECLAIM_NOT_RECORDED = "Its worktree was removed and its branch kept, but the deck could not record it, so it may be listed again.";
export const DISMISSED = "Dismissed.";

export const REFUSED_AGENT_CALLER = "Refused: this request came from an agent in a deck pane, and reclaiming or dismissing interrupted work is left to the person. Do it from the deck or from a terminal.";
export const REFUSED_BAD_REQUEST = "Refused: name the items to dismiss, or dismiss all of them.";
export const REFUSED_TOO_LARGE = "Refused: the request names too many items, or an id that is too long.";
export const REFUSED_UNREADABLE = "Refused: the deck could not read its work journal.";
export const REFUSED_INCOMPLETE_READ = "Refused: the deck could not read its whole work journal, so it cannot be sure of this item.";
export const REFUSED_NOT_LISTED = "No interrupted item has that id, name or branch.";
export const REFUSED_AMBIGUOUS = "More than one interrupted item matches; use the item's id.";
export const REFUSED_PLAIN_AGENT = "Refused: the deck did not create a worktree for this agent, so there is nothing to reclaim. Dismiss it instead.";
export const REFUSED_NO_WORKTREE = "Refused: its worktree is not on disk.";
export const REFUSED_NOT_LINKED_WORKTREE = "Refused: its directory is not a linked Git worktree.";
export const REFUSED_BRANCH_MISMATCH = "Refused: its worktree has another branch checked out than the one it was started on.";
export const REFUSED_NOT_OWNED = "Refused: the deck did not create this worktree.";
export const REFUSED_NEWER_RUN = "Refused: a later run used this worktree, so reclaiming this older item could remove that run's work. Dismiss this item instead.";
export const REFUSED_STILL_IN_USE = "Refused: an agent is still running in its worktree.";
export const REFUSED_IN_USE_UNKNOWN = "Refused: the deck could not tell whether an agent is still running in its worktree.";
export const REFUSED_NOT_RECORDED = "Refused: the deck could not record the dismissal.";
export const REFUSED_UNKNOWN = "Refused.";

export const NOTHING_INTERRUPTED = "No interrupted work.";
export const LIST_INCOMPLETE = "The deck could not read its whole work journal, so an item may be missing or out of date.";
/**
 * Verification finding N4 — the banner of a daemon whose list came back
 * empty but incomplete: there may be work it could not read, so the banner
 * offers Show rather than leaving that daemon with no way in. The TUI's
 * footer row says the same for its daemon.
 */
export const INTERRUPTED_UNREAD = "The deck could not read its whole work journal, so some interrupted work may not be listed.";
export const LABEL_ISSUE = "Issue";
export const LABEL_BRANCH = "Branch";
export const LABEL_WORKTREE = "Worktree";
export const LABEL_FOLDER = "Folder";
export const LABEL_STARTED_BY = "Started by";
export const LABEL_LAST_ENTRY = "Last entry";
export const LABEL_GIT = "Git";
export const LABEL_REPORT = "Report";
export const LABEL_PROGRESS = "Progress";
export const ITEM_CUT = "Some of this item's text is too long to show in full.";
export const LABEL_NESTED_WORKTREE = "Holds worktree";
export const REPORT_CUT = "This report is longer than shown; this is its start.";
export const REPORT_UNREADABLE = "This report cannot be shown:";
export const NOTHING_DISMISSED = "Nothing was dismissed.";
export const DISMISS_STOPPED_REPLACED = "the deck daemon was replaced. List the interrupted work again.";
export const DISMISS_STOPPED_FAILED = "the deck daemon did not answer. List the interrupted work again.";
export const UNSUPPORTED = "The running deck daemon does not support interrupted work. Stop it with `dot-agent-deck daemon restart`, then start one from this build: open the TUI, or press Start daemon in the desktop app.";
export const DAEMON_FAILED = "The deck daemon could not answer:";
export const MAX_SHOWN_CHARS = 300;
/** `MAX_SHOWN_REPORTS`: the most reports one page shows; Previous reports falls back on it with no page to go back to. */
export const MAX_SHOWN_REPORTS = 16;

/** `interrupted_footer`: "N interrupted items from before the restart", singular for one. */
export function interruptedFooter(count: number): string {
  return count === 1 ? "1 interrupted item from before the restart" : `${count} interrupted items from before the restart`;
}

/** `dismiss_all_question`, without the terminal's trailing space. */
export function dismissAllQuestion(count: number): string {
  return `${DISMISS_ALL_ACTION}: ${interruptedFooter(count)}? [y/N]`;
}

/** `kind_label`. */
export function kindLabel(kind: InterruptedItemKind | string): string {
  switch (kind) {
    case "unit": return "Unit";
    case "orchestration": return "Orchestration";
    case "issue_dispatch": return "Issue dispatch";
    case "agent": return "Agent";
    default: return displayValue(kind);
  }
}

/** `format_time`: `YYYY-MM-DD HH:MM UTC`. */
export function formatTime(ms: number): string {
  const at = new Date(ms);
  if (!Number.isFinite(ms) || Number.isNaN(at.getTime())) return "an unknown time";
  const two = (n: number) => String(n).padStart(2, "0");
  return `${at.getUTCFullYear()}-${two(at.getUTCMonth() + 1)}-${two(at.getUTCDate())} ${two(at.getUTCHours())}:${two(at.getUTCMinutes())} UTC`;
}

/** `kept_until` (decision D7). */
export function keptUntil(ms: number | null | undefined): string {
  return typeof ms === "number" ? `${KEPT_UNTIL} ${formatTime(ms)}` : "kept, with no removal date yet";
}

/** `report_range_sentence`. */
export function reportRangeSentence(first: number, last: number, total: number): string {
  return first === last ? `Report ${first} of ${total}.` : `Reports ${first} to ${last} of ${total}.`;
}

/** `omitted_sentence`. */
export function omittedSentence(count: number): string {
  return count === 1 ? "1 older item is not shown." : `${count} older items are not shown.`;
}

/** `progress_line`. */
export function progressLine(progress: InterruptedProgress): string {
  const parts = [progress.attempts === 1 ? "1 attempt" : `${progress.attempts} attempts`];
  if (progress.roles?.length) parts.push(`roles ${progress.roles.join(", ")}`);
  if (progress.delegations > 0) {
    parts.push(`${progress.delegations_done} of ${progress.delegations} ${progress.delegations === 1 ? "delegation" : "delegations"} done`);
  }
  return parts.join("; ");
}

/** `git_facts_line`: a fact that could not be read is left out. */
export function gitFactsLine(git: InterruptedGitFacts): string {
  const facts: string[] = [];
  if (git.worktree_exists === true) facts.push("worktree on disk");
  if (git.worktree_exists === false) facts.push("worktree gone");
  if (git.dirty === true) facts.push("uncommitted changes");
  if (git.dirty === false) facts.push("no uncommitted changes");
  if (git.unpushed_commits === 1) facts.push("1 commit on no remote");
  else if (typeof git.unpushed_commits === "number" && git.unpushed_commits > 1) facts.push(`${git.unpushed_commits} commits on no remote`);
  if (git.branch_exists === true) facts.push("branch kept");
  if (git.branch_exists === false) facts.push("branch gone");
  facts.push(git.pr === "open" ? "pull request open" : git.pr === "merged" ? "pull request merged" : git.pr === "closed" ? "pull request closed" : git.pr === "none" ? "no pull request" : "pull request unknown");
  return facts.join(", ");
}

/** `unpushed_sentence` (decision D5): `undefined` when there are none. */
export function unpushedSentence(count: number | null | undefined): string | undefined {
  if (typeof count !== "number" || count <= 0) return undefined;
  return count === 1 ? "1 commit on its branch is on no remote; the branch keeps it." : `${count} commits on its branch are on no remote; the branch keeps them.`;
}

const RECLAIM_SENTENCES: Record<string, string> = {
  removed: RECLAIM_REMOVED,
  "would-remove": RECLAIM_WOULD_REMOVE,
  "kept-dirty": RECLAIM_KEPT_DIRTY,
  "kept-holds-worktrees": RECLAIM_KEPT_HOLDS_WORKTREES,
  "kept-could-not-check": RECLAIM_KEPT_COULD_NOT_CHECK,
  "remove-failed": RECLAIM_REMOVE_FAILED,
  "timed-out": RECLAIM_TIMED_OUT,
  "kept-changed": RECLAIM_KEPT_CHANGED,
  "kept-busy": RECLAIM_KEPT_BUSY,
  "kept-could-not-lock": RECLAIM_KEPT_COULD_NOT_LOCK,
};

const REFUSAL_SENTENCES: Record<string, string> = {
  "agent-caller": REFUSED_AGENT_CALLER,
  "bad-request": REFUSED_BAD_REQUEST,
  "too-large": REFUSED_TOO_LARGE,
  unreadable: REFUSED_UNREADABLE,
  "incomplete-read": REFUSED_INCOMPLETE_READ,
  "not-listed": REFUSED_NOT_LISTED,
  ambiguous: REFUSED_AMBIGUOUS,
  "plain-agent": REFUSED_PLAIN_AGENT,
  "no-worktree": REFUSED_NO_WORKTREE,
  "not-linked-worktree": REFUSED_NOT_LINKED_WORKTREE,
  "branch-mismatch": REFUSED_BRANCH_MISMATCH,
  "not-owned": REFUSED_NOT_OWNED,
  "newer-run": REFUSED_NEWER_RUN,
  "still-in-use": REFUSED_STILL_IN_USE,
  "in-use-unknown": REFUSED_IN_USE_UNKNOWN,
  "not-recorded": REFUSED_NOT_RECORDED,
};

/** `refusal_sentence`; a reason this app does not know shows the daemon's own words, escaped. */
function refusalText(refusal: Refusal): string {
  return REFUSAL_SENTENCES[refusal.reason] ?? displayValue(refusal.message);
}

/** `interrupted_cli::reclaim_message`: the sentence for a reclaim's outcome. */
export function reclaimMessage(outcome: ReclaimOutcome): string {
  if (outcome.refusal) return refusalText(outcome.refusal);
  if (outcome.verdict === "removed" && !outcome.recorded) return RECLAIM_NOT_RECORDED;
  return RECLAIM_SENTENCES[outcome.verdict] ?? displayValue(outcome.message);
}

/** Whether a dry run says the worktree would be removed: the only verdict Reclaim's confirm is offered for. */
export function wouldRemove(outcome: ReclaimOutcome): boolean {
  return outcome.verdict === "would-remove" && !outcome.refusal;
}

/** The TUI's `dismiss_message`: the sentence for a dismissal's outcome — a refusal of one of its ids, or whether anything was dismissed — then why a dismissal sent in batches stopped, when it did. */
export function dismissMessage(outcome: DismissOutcome): string {
  if (outcome.refusal) return refusalText(outcome.refusal);
  const refused = outcome.refused[0];
  const said = refused ? refusalText(refused.refusal) : outcome.dismissed.length === 0 && !outcome.dismissed_omitted ? NOTHING_DISMISSED : DISMISSED;
  return outcome.stopped ? `${said} ${dismissStoppedSentence(outcome.stopped)}` : said;
}

/** `dismiss_stopped_sentence`: why a dismissal sent in batches stopped. A refused batch was not dismissed; one whose daemon was replaced, or that got no answer, may have been. */
export function dismissStoppedSentence(stopped: DismissStopped): string {
  const n = stopped.not_sent;
  const unsure = stopped.reason === "replaced" || stopped.reason === "failed";
  const [one, many] = unsure
    ? ["1 more item may not have been dismissed", "more items may not have been dismissed"]
    : ["1 more item was not dismissed", "more items were not dismissed"];
  const lead = n === 1 ? one : `${n} ${many}`;
  if (stopped.reason === "replaced") return `${lead}: ${DISMISS_STOPPED_REPLACED}`;
  if (stopped.reason === "failed") return `${lead}: ${DISMISS_STOPPED_FAILED}`;
  if (stopped.reason === "refused") return `${lead}. ${REFUSAL_SENTENCES[stopped.refusal ?? ""] ?? REFUSED_UNKNOWN}`;
  return `${lead}.`;
}

// ---------------------------------------------------------------------------
// Escaping (`untrusted_text::escape_control_and_bidi`, `display_value`).
// ---------------------------------------------------------------------------

function isControl(code: number): boolean {
  return code <= 0x1f || (code >= 0x7f && code <= 0x9f);
}

function isBidiFormat(code: number): boolean {
  return (code >= 0x202a && code <= 0x202e) || (code >= 0x2066 && code <= 0x2069) || code === 0x200e || code === 0x200f || code === 0x061c;
}

/**
 * `escape_control_and_bidi`: every control and bidi formatting character
 * written as its escape (`\u{1b}`, `\u{202e}`, `\n`), as the CLI and the TUI
 * show it, so none can reorder the text around it or break a line. A line or
 * paragraph separator (`U+2028`, `U+2029`), which a browser breaks a line on,
 * is escaped too.
 */
export function escapeControlAndBidi(value: string): string {
  let out = "";
  for (const ch of value) {
    const code = ch.codePointAt(0) ?? 0;
    if (isControl(code)) {
      out += ch === "\t" ? "\\t" : ch === "\n" ? "\\n" : ch === "\r" ? "\\r" : `\\u{${code.toString(16)}}`;
    } else if (isBidiFormat(code) || code === 0x2028 || code === 0x2029) {
      out += `\\u{${code.toString(16)}}`;
    } else {
      out += ch;
    }
  }
  return out;
}

/** `display_value`: escaped, and cut to {@link MAX_SHOWN_CHARS} characters, ending in `…` when it was. */
export function displayValue(value: string | null | undefined): string {
  const chars = [...escapeControlAndBidi(value ?? "")];
  return chars.length > MAX_SHOWN_CHARS ? `${chars.slice(0, MAX_SHOWN_CHARS).join("")}…` : chars.join("");
}

/**
 * A report body's lines as the TUI shows them: tabs as spaces and each line
 * escaped. The daemon frames a body as untrusted text an agent wrote; it is
 * shown as framed.
 */
export function reportLines(body: string): string[] {
  return body.replace(/\r\n/g, "\n").split("\n").map((line) => escapeControlAndBidi(line.replace(/\t/g, " ")));
}

// ---------------------------------------------------------------------------
// The list, as the TUI shows it.
// ---------------------------------------------------------------------------

/** The items in the order shown: work (units, orchestrations, issue dispatches) first, then plain agents ("Other agents", decision D1), each as the daemon listed it. */
export function orderedItems(list: InterruptedList | undefined): InterruptedItem[] {
  if (!list) return [];
  return [...list.items.filter((item) => item.kind !== "agent"), ...list.items.filter((item) => item.kind === "agent")];
}

/** How many items the daemon listed, those it left out included. */
export function listedCount(list: InterruptedList | undefined): number {
  return list ? list.items.length + (list.omitted ?? 0) : 0;
}

/** Whether a daemon's list names work or, being incomplete, may hold work it could not read: what draws its banner. */
export function mayHaveInterrupted(list: InterruptedList | undefined): boolean {
  return listedCount(list) > 0 || list?.incomplete === true;
}

/** An item's name, or its branch, or its id, escaped. */
export function itemName(item: InterruptedItem): string {
  return displayValue(item.name ?? item.branch ?? item.id);
}

/** The label of an item's directory: a plain agent's is a folder the deck did not create. */
export function directoryLabel(item: InterruptedItem): string {
  return item.kind === "agent" ? LABEL_FOLDER : LABEL_WORKTREE;
}

/** "Last entry: <kind>, <time>". */
export function lastEntryLine(item: InterruptedItem): string {
  return `${LABEL_LAST_ENTRY}: ${displayValue(item.last_entry.kind)}, ${formatTime(item.last_entry.ts_ms)}`;
}

/** "Progress: …". */
export function progressFact(item: InterruptedItem): string {
  return `${LABEL_PROGRESS}: ${displayValue(progressLine(item.progress))}`;
}
