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

interface InterruptedLastEntry { kind: string; ts_ms: number }

interface InterruptedItemReport {
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

type PrFact = "open" | "merged" | "closed" | "none" | "unknown";

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
  /** PRD #1790 stage 3: the agents that served the item, each with whether it can be restored. Absent from a daemon before stage 3. */
  agents?: ItemAgent[];
  agents_omitted?: number;
}

/**
 * PRD #1790 stage 3 — whether an agent can be restored (`RestoreVerdict`):
 * `resume` with its conversation, `start_over` without it, or `none`. A
 * `kind` this app does not know is a newer daemon's, and reads as unknown.
 */
export type RestoreVerdict =
  | { kind: "resume" }
  | { kind: "start_over"; reason: string }
  | { kind: "none"; reason: string }
  | { kind: string; reason?: string };

/** PRD #1790 stage 3 — one agent of an interrupted item (`ItemAgent`). */
export interface ItemAgent {
  attempt: string;
  name?: string | null;
  agent_type?: string | null;
  role?: string | null;
  folder?: string | null;
  restore: RestoreVerdict;
}

/** PRD #1790 stage 3 — which note a restored agent receives first (`RestoreMode`). */
export type RestoreMode = "continue" | "only";

/** One agent a restore or start-over started (`RestoredAgent`). */
export interface RestoredAgent {
  attempt: string;
  agent_id: string;
  pane_id: string;
  name?: string | null;
  first_message_posted?: boolean;
  recorded?: boolean;
}

/** One refusal of a restore or start-over (`RestoreRefused`); `attempt` is `null` for the whole request. */
export interface RestoreRefused {
  attempt?: string | null;
  reason: string;
  message: string;
}

/** The final answer to a restore (`RestoreOutcome`). */
export interface RestoreOutcome {
  dry_run?: boolean;
  restored: RestoredAgent[];
  refused: RestoreRefused[];
  would_restore?: string[];
  git?: InterruptedGitFacts | null;
}

/** The answer to a start-over (`StartOverOutcome`). */
export interface StartOverOutcome {
  dry_run?: boolean;
  started?: RestoredAgent | null;
  refused?: RestoreRefused | null;
  would_start?: boolean;
  git?: InterruptedGitFacts | null;
}

export interface InterruptedList {
  items: InterruptedItem[];
  incomplete?: boolean;
  omitted?: number;
}

interface ShownReport {
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

type RefusalReason =
  | "agent-caller" | "bad-request" | "too-large" | "unreadable" | "incomplete-read" | "not-listed" | "ambiguous"
  | "plain-agent" | "no-worktree" | "not-linked-worktree" | "branch-mismatch" | "not-owned" | "newer-run"
  | "owner-unconfirmed" | "still-in-use" | "in-use-unknown" | "not-recorded" | "unknown";

interface Refusal { reason: RefusalReason | string; message: string }

type ReclaimVerdict =
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
interface DismissStopped {
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
/** A restore answer: `restored` and `refused` arrays. */
export const isRestoreOutcome = (value: unknown): value is RestoreOutcome => isRecord(value) && Array.isArray(value.restored) && Array.isArray(value.refused);
/** A start-over answer: a `started` agent or `null` (the daemon always sends it), and a refusal or `null` when present. */
export const isStartOverOutcome = (value: unknown): value is StartOverOutcome => isRecord(value)
  && "started" in value && (value.started === null || isRecord(value.started))
  && (value.refused === undefined || value.refused === null || isRecord(value.refused));

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
  /**
   * PRD #1790 stage 3 — open the restore question about the item `item`
   * names, or the selected one: nothing is sent until one of its choices is
   * confirmed. Answers why not, as the others do.
   */
  askRestore: (item?: string) => string | undefined;
  /** The question's choices: Restore and continue, Restore only, Start over in this worktree. */
  restoreAndContinue: () => string | undefined;
  restoreOnly: () => string | undefined;
  startOver: () => string | undefined;
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
export const REFUSED_OWNER_UNCONFIRMED = "Refused: the deck cannot confirm this worktree is still this item's, for example because an older version of the deck created it. Dismiss this item instead, and remove the worktree with Git if you no longer need it.";
export const REFUSED_STILL_IN_USE = "Refused: an agent is still running in its worktree.";
export const REFUSED_IN_USE_UNKNOWN = "Refused: the deck could not tell whether an agent is still running in its worktree.";
export const REFUSED_NOT_RECORDED = "Refused: the deck could not record the dismissal.";
export const REFUSED_UNKNOWN = "Refused.";

export const NOTHING_INTERRUPTED = "No interrupted work.";
/** PR #2431 round 4 — a list request failed while an earlier answer is still shown; the reason follows. */
export const LIST_REFRESH_FAILED = "The list could not be refreshed, so it may be out of date.";
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
/** PRD #1790 stage 3: the restore question's words, copies of the Rust constants of the same names. */
export const RESTORE_ACTION = "Restore";
export const RESTORE_AND_CONTINUE = "Restore and continue";
export const RESTORE_ONLY = "Restore only";
export const START_OVER = "Start over in this worktree";
export const RESTORE_AND_CONTINUE_EXPLANATION = "It checks what already happened and carries on where it stopped.";
export const RESTORE_ONLY_EXPLANATION = "It comes back and waits until you tell it to continue.";
export const RESTORE_VERDICT_RESUME = "Can be restored with its conversation.";
export const LABEL_AGENT = "Agent";
export const LABEL_ATTEMPT = "Attempt";
export const RESTORE_WOULD = "Restore would bring the agent back with its conversation.";
export const RESTORE_PR_OPEN = "Its branch has an open pull request, which the agent may change.";
export const START_OVER_WOULD = "Start over would start the agent again in this worktree, without its conversation.";
export const RESTART_NOTE_NOT_SENT = "The deck could not give the restored agent its restart note, so it does not know the deck restarted. Tell it before it continues.";
export const SAVED_TASK_NOT_SENT = "The deck could not give the new agent its saved task. Give it the task yourself.";
export const RESTORE_NOT_RECORDED = "The deck could not record this, so the item may be listed again. Dismiss it if it is.";
export const NOTHING_RESTORED = "Nothing was restored.";
export const RESTORE_UNSUPPORTED = "The running deck daemon does not support restoring agents. Stop it with `dot-agent-deck daemon restart`, then start one from this build: open the TUI, or press Start daemon in the desktop app.";
export const NO_AGENT_TO_RESTORE = "This item lists no agent to restore.";
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
  "owner-unconfirmed": REFUSED_OWNER_UNCONFIRMED,
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
function dismissStoppedSentence(stopped: DismissStopped): string {
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
// PRD #1790 stage 3: restoring an agent (copies of `src/interrupted_work.rs`).
// ---------------------------------------------------------------------------

/** `restore_question`. */
export function restoreQuestion(name: string): string {
  return `Restore ${name} with its conversation?`;
}

/** `start_over_question`. */
export function startOverQuestion(name: string): string {
  return `Start ${name} over in this worktree, without its conversation?`;
}

/** `restored_sentence`. */
function restoredSentence(name: string): string {
  return `Restored ${name} with its conversation.`;
}

/** `started_over_sentence`. */
function startedOverSentence(name: string): string {
  return `Started ${name} over in this worktree.`;
}

const RESTORE_REASON_SENTENCES: Record<string, string> = {
  "agent-cannot-resume": "The agent cannot be restored with its conversation yet.",
  "no-session-id": "The deck does not know which conversation the agent was in.",
  "session-id-invalid": "The conversation id the deck holds for the agent is not one it can resume.",
  "no-launch-record": "The deck has no record of how the agent was started.",
  "ambiguous-record": "The deck's records do not say for certain how the agent was started.",
  "folder-moved": "The agent's folder was moved, is gone, or others can write to it.",
  "folder-unknown": "The deck does not know which folder the agent ran in.",
  "orchestration-later": "Orchestrations come back in a later version.",
  "clear-worker": "This worker starts fresh for each task and comes back with its orchestration.",
  "default-shell": "The pane ran a shell, not an agent, so there is no conversation to restore.",
  "incomplete-read": "The deck could not read its whole work journal or all its records of how agents were started.",
  "not-interrupted": "The restart did not stop this agent.",
  "not-resumed": "The deck tried to restore the agent's conversation and could not.",
  "old-process-unknown": "An older version of the deck started this agent, so the deck cannot tell whether its old process is gone.",
  restored: "The deck already brought this agent back.",
};
const RESTORE_REASON_UNKNOWN = "The agent cannot be restored.";

/** `restore_reason_sentence`: a reason of a newer daemon's reads as unknown. */
export function restoreReasonSentence(reason: string | undefined): string {
  return (reason !== undefined && Object.hasOwn(RESTORE_REASON_SENTENCES, reason) ? RESTORE_REASON_SENTENCES[reason] : undefined) ?? RESTORE_REASON_UNKNOWN;
}

/** `restore_verdict_sentence`. */
export function restoreVerdictSentence(verdict: RestoreVerdict): string {
  if (verdict.kind === "resume") return RESTORE_VERDICT_RESUME;
  if (verdict.kind === "start_over") return `${restoreReasonSentence(verdict.reason)} It can be started again without it: ${START_OVER}.`;
  if (verdict.kind === "none") return restoreReasonSentence(verdict.reason);
  return RESTORE_REASON_UNKNOWN;
}

const RESTORE_REFUSAL_SENTENCES: Record<string, string> = {
  "agent-caller": "Refused: this request came from an agent in a deck pane, and restoring agents is left to the person. Do it from the deck or from a terminal.",
  "bad-request": "Refused: name the agent to start over.",
  unsupported: "Refused: the deck cannot restore agents on this system.",
  unreadable: REFUSED_UNREADABLE,
  "incomplete-read": "Refused: the deck could not read its whole work journal or all its records of how agents were started, so it cannot be sure how the agent was started.",
  "not-listed": REFUSED_NOT_LISTED,
  ambiguous: REFUSED_AMBIGUOUS,
  "not-an-agent": "Refused: that agent is not one of this item's.",
  "not-restorable": "Refused: this agent cannot be brought back that way. Show the item to see what it offers.",
  "ambiguous-record": "Refused: the deck's records do not say for certain how the agent was started.",
  "no-launch-record": "Refused: the deck has no record of how the agent was started.",
  "folder-moved": "Refused: the agent's folder was moved away.",
  "folder-unknown": "Refused: the deck does not know which folder the agent ran in.",
  "folder-missing": "Refused: the agent's folder is not on disk.",
  "folder-unsafe": "Refused: another account could change the agent's folder, or the path to it.",
  "no-session-id": "Refused: the deck does not know which conversation the agent was in.",
  "session-id-invalid": "Refused: the conversation id the deck holds for the agent is not one it can resume.",
  "still-running": "Refused: the agent's old process, or a process it started, is still running. Stop it first, so two agents do not write to one conversation.",
  "old-process-unknown": "Refused: the deck cannot tell whether the agent's old process is gone: an older version of the deck started it, or the system did not let the deck look. Start it over instead.",
  "already-restoring": "Refused: this agent is already being restored or started over.",
  "shutting-down": "Refused: the deck is shutting down or restarting.",
  "spawn-failed": "Refused: the deck could not start the agent.",
  "not-resumed": "The agent started but did not come back into its conversation, so the deck stopped it before sending it anything. You can start it over in this worktree instead.",
  "task-unsafe": "Refused: the saved task file cannot be trusted (it is a link, not a plain file, another account's, open to other accounts, or too large).",
  "no-saved-task": "Refused: the unit's saved task is not there.",
  "session-in-use": "Refused: another agent the deck runs is in this conversation now, or is being restored into it, or an agent the deck could not confirm it stopped may be in it, so two agents would write to one conversation.",
  "too-many-pending": "Refused: too many restores are waiting for their agents to start. Try again in a minute.",
  "bad-selection": "Refused: the request named an agent twice, named too many agents, or used an id that is too long.",
  "command-reopens": "Refused: the agent's command reopens an earlier conversation, so starting it over would not start fresh. Start it yourself without that option.",
  "not-fresh": "The deck could not confirm that the agent started a new conversation rather than its old one, so it stopped the agent before sending it anything.",
  "not-stopped": "The deck stopped the agent it had just started, but could not confirm that all of it stopped. Make sure it is not running before you try again.",
  "pr-open": "Refused: its branch has an open pull request, and starting the agent over would repeat its task beside it. Restore it with its conversation instead, or close the pull request first.",
  "directory-in-use": "Refused: newer work is running in this agent's folder now. Stop it first, so two agents do not work in one folder.",
  "note-refused": "Refused: the deck's inbox cannot take the restart note now, so nothing was started. Try again in a minute.",
  "note-not-delivered": "The agent came back into its conversation, but the deck could not give it its restart note, so it stopped the agent again. The item is still listed; try again.",
  "task-not-delivered": "The agent started over, but the deck could not give it its saved task, so it stopped the agent again. The item is still listed; try again.",
  "not-recorded": "The agent started, but the deck could not record it in its work journal, so it stopped the agent again. The item is still listed; try again.",
  "restore-busy": "Refused: another restore or start over is in progress. Try again in a moment.",
};

/** `restore_refusal_sentence`: a refusal of a newer daemon's reads as {@link REFUSED_UNKNOWN}. */
export function restoreRefusalSentence(reason: string): string {
  return (Object.hasOwn(RESTORE_REFUSAL_SENTENCES, reason) ? RESTORE_REFUSAL_SENTENCES[reason] : undefined) ?? REFUSED_UNKNOWN;
}

/** The name the restore question uses for an agent: its own, or its item's. */
export function agentName(agent: ItemAgent, item: InterruptedItem): string {
  return agent.name ? displayValue(agent.name) : itemName(item);
}

/** Whether a verdict offers a choice in the question: a resume, or a start-over. */
export function offersChoice(verdict: RestoreVerdict): boolean {
  return verdict.kind === "resume" || verdict.kind === "start_over";
}

/**
 * The agent the restore question asks about, as the TUI picks it: the item's
 * first agent that is offered a choice, else its first agent; `undefined`
 * for an item from a daemon that lists no agents.
 */
export function restoreAgent(item: InterruptedItem): ItemAgent | undefined {
  return item.agents?.find((agent) => offersChoice(agent.restore)) ?? item.agents?.[0];
}

/** `restore_refusal_text`: the shared sentence for the refusal's code, or for a code this app does not know, the daemon's own words, escaped. */
function restoreRefusalText(refused: RestoreRefused): string {
  return Object.hasOwn(RESTORE_REFUSAL_SENTENCES, refused.reason) ? RESTORE_REFUSAL_SENTENCES[refused.reason] : displayValue(refused.message);
}

/** `restore_git_note`: {@link RESTORE_PR_OPEN} when the item's branch has an open pull request. */
export function restoreGitNote(git: InterruptedGitFacts | null | undefined): string | undefined {
  return git?.pr === "open" ? RESTORE_PR_OPEN : undefined;
}

/**
 * The TUI's sentence for a restore's answer: `interrupted_cli::render_restore`'s
 * lines, what it prints and then what it warns, on one line.
 */
export function restoreMessage(outcome: RestoreOutcome): string {
  const said: string[] = [];
  const warned: string[] = [];
  for (const attempt of outcome.would_restore ?? []) said.push(`${RESTORE_WOULD} ${displayValue(attempt)}`);
  const note = restoreGitNote(outcome.git);
  if (note) said.push(note);
  for (const agent of outcome.restored) {
    said.push(restoredSentence(displayValue(agent.name ?? "")), `${LABEL_ATTEMPT}: ${displayValue(agent.attempt)}`);
    if (!agent.first_message_posted) warned.push(RESTART_NOTE_NOT_SENT);
    if (!agent.recorded) warned.push(RESTORE_NOT_RECORDED);
  }
  for (const refused of outcome.refused) {
    warned.push(typeof refused.attempt === "string" ? `${displayValue(refused.attempt)}: ${restoreRefusalText(refused)}` : restoreRefusalText(refused));
  }
  const acted = outcome.restored.length > 0 || (outcome.would_restore?.length ?? 0) > 0;
  if (!acted && outcome.refused.length === 0) warned.push(NOTHING_RESTORED);
  return [...said, ...warned].join(" ");
}

/** The TUI's sentence for a start-over's answer (`interrupted_cli::render_start_over`). */
export function startOverMessage(outcome: StartOverOutcome): string {
  if (outcome.refused) return restoreRefusalText(outcome.refused);
  if (outcome.would_start) {
    const note = restoreGitNote(outcome.git);
    return note ? `${START_OVER_WOULD} ${note}` : START_OVER_WOULD;
  }
  const agent = outcome.started;
  if (!agent) return NOTHING_RESTORED;
  const lines = [startedOverSentence(displayValue(agent.name ?? ""))];
  if (!agent.first_message_posted) lines.push(SAVED_TASK_NOT_SENT);
  if (!agent.recorded) lines.push(RESTORE_NOT_RECORDED);
  return lines.join(" ");
}

/**
 * `restored_cleanly`: whether a restore or start-over brought its agents back
 * with nothing left to tell — none refused, each given its first message and
 * recorded. The dialog then closes on the dashboard that shows the agent, as
 * the TUI's overlay does.
 */
export function restoredCleanly(started: readonly RestoredAgent[], refused: boolean): boolean {
  return !refused && started.length > 0 && started.every((agent) => agent.first_message_posted === true && agent.recorded === true);
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

/**
 * PR #2431 review — the item an item's details show: the show's answer when
 * there is one, with the Git facts the list read, since a show answers its
 * item without them. The TUI's details keep them the same way.
 */
export function detailsItem(shown: InterruptedItem | undefined, listed: InterruptedItem | undefined): InterruptedItem | undefined {
  if (!shown) return listed;
  return shown.git || !listed?.git ? shown : { ...shown, git: listed.git };
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
