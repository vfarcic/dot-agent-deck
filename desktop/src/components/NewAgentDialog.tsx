import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { ArrowUp, Check, Folder, FolderGit2, Loader2, Plus, Server, Trash2, X } from "lucide-react";
import { LaunchCleanupError } from "../lib/actionError";
import { CleanupWarning } from "./CleanupWarning";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { useInertBackground } from "../hooks/useInertBackground";
import {
  ambiguousOrchestrationReason,
  AUTHORING_MODES,
  authoringModes,
  deckChoices,
  directoryLabel,
  directoryListingOptions,
  filterDirectoryEntries,
  fleetLists,
  isDeckGoneError,
  liveOrchestrationDirectories,
  liveOrchestrationTitles,
  NEW_AGENT_APPEAR_TIMEOUT_MS,
  ORCHESTRATION_TITLE_TAKEN,
  orchestrationModeId,
  orchestrationModes,
  orchestrationRunTitle,
  preselectedDeck,
  resolveAuthoringCommand,
  SAME_DIRECTORY_ORCHESTRATION,
  seedCommand,
  suggestOrchestrationName,
  type DeckChoice,
} from "../lib/newAgent";
import type { AuthoringKind, DaemonOrchestration, DeckDirectoryEntry, DeckDirectoryListing, DeckRuntimeState, NewAgentOption, NewAgentOptions, NewAgentOrchestrations } from "../types";
import type { NewAgentVoiceChannel, VoiceDispatchTarget } from "../lib/voiceActions";
import { draftHasEdits, draftWorthKeeping, type NewAgentDraft } from "../lib/newAgentDraft";

/**
 * What the dialog needs from the runtime. The two queries are REQUIRED here
 * although they are optional on `DeckRuntimeState`: the overview renders no
 * entry point into this dialog for a runtime without them.
 */
export type NewAgentRuntime = Pick<DeckRuntimeState, "fleet" | "runAction" | "clearError"> & Required<Pick<DeckRuntimeState, "listDirectories" | "newAgentOptions">> & Pick<DeckRuntimeState, "newAgentOrchestrations">;

export interface NewAgentDialogProps {
  runtime: NewAgentRuntime;
  /** The deck the flow was opened from — a deck header's affordance — which the deck field preselects — and chooses — when it can take a spawn. */
  initialDeckId?: string;
  /**
   * Issue #1247 — what the form held when it was last closed without being
   * discarded, replayed on open against fresh answers from the deck (see
   * the open effect's "A saved draft"). Read once, on mount.
   */
  draft?: NewAgentDraft;
  /**
   * Every way out: `draft` is what to keep for the next open, or `undefined`
   * when there is nothing to keep — the form was discarded, it was empty, or
   * it had already been started. The caller stores it; the dialog holds no
   * state across mounts. See `lib/newAgentDraft.ts` for which actions keep it.
   */
  onClose: (draft?: NewAgentDraft) => void;
  /** The started agent is listed by its deck: the caller opens its pane. */
  onAppeared: (target: { deckId: string; agentId: string }) => void;
  /** The deck accepted the start and has not listed the agent within the bound. */
  onNotAppeared: (report: { deckName: string; agentName: string }) => void;
  /** Test seam; production uses {@link NEW_AGENT_APPEAR_TIMEOUT_MS}. */
  appearTimeoutMs?: number;
  /**
   * PRD #1223 U5 — where the dialog publishes its close for a route that is
   * not one of its own controls: voice's `close`. The function closes exactly
   * as the X does and answers `undefined`, or — while a start is in flight —
   * closes nothing and answers the sentence the X's `title` carries.
   */
  closeRequest?: { current: (() => string | undefined) | undefined };
  /**
   * PRD #1223 — where the dialog publishes its directory browser for voice:
   * what it shows (the declaration a spoken `dir_ref` resolves against) and
   * its three moves. Written every render while mounted, cleared on unmount.
   */
  voice?: NewAgentVoiceChannel;
}

type Listing = Extract<DeckDirectoryListing, { kind: "listing" }>;

/** One row of the directory panel: the parent (`..`), or a subdirectory. */
type DirectoryRow = { kind: "up"; path: string } | { kind: "entry"; entry: DeckDirectoryEntry };

/** #1247 — the part of a saved draft that hangs off its deck, replayed once that deck answers. */
type Resume = Pick<NewAgentDraft, "browsing" | "directory" | "mode">;

/**
 * What became of one listing request: it landed and is on screen, it failed,
 * the deck cannot list at all, or a later request (or the deck leaving)
 * superseded it — in which case nothing it would have done is done.
 */
type ListingOutcome = { kind: "listed"; listing: Listing } | { kind: "failed" } | { kind: "unsupported" } | { kind: "stale" };

/**
 * Issue #1240 — how long the filter waits after the last keystroke before it
 * asks the deck to search a directory its cap truncated. Short enough to feel
 * live, long enough that typing a word is one request rather than one per key.
 */
export const DECK_SEARCH_DEBOUNCE_MS = 250;

/**
 * Which row a listing that lands should put the cursor on: a path, and — for
 * a row the user was on — its name too. Issue #1240 made the name matter: a
 * symlink is listed by its target, so a link and the directory it leads to (or
 * two links to one place) share a path and differ only by name.
 */
type Focus = { path: string; name?: string };

/**
 * The index in `entries` of `focus`, or -1: the exact row when a name is
 * given, and otherwise the real directory at that path before any link to it —
 * the directory just left, when going up, is the real one.
 */
function focusIndex(entries: readonly DeckDirectoryEntry[], focus: Focus | undefined): number {
  if (!focus) return -1;
  if (focus.name !== undefined) return entries.findIndex((entry) => entry.path === focus.path && entry.displayName === focus.name);
  const real = entries.findIndex((entry) => entry.path === focus.path && !entry.isSymlink);
  return real >= 0 ? real : entries.findIndex((entry) => entry.path === focus.path);
}

/** Issue #1240 — the truncation hints, in the dialog's own words. */
export const TRUNCATED_NO_SEARCH = "Not every subdirectory is listed: the deck stopped at its limit, and the ones past it cannot be chosen here.";
export const TRUNCATED_SEARCHABLE = "Not every subdirectory is listed: the deck stopped at its limit. Filter by name and the deck searches all of them.";
export const SEARCH_TRUNCATED = "More subdirectories match this filter than the deck lists at once. Narrow the filter to find the one you want.";

/**
 * The Mode row's first chip — a plain agent. Then, in the TUI cycler's order,
 * one chip per orchestration the directory's project defines on the deck (PRD
 * #1223 M6, from {@link orchestrationModes}), then the authoring kinds the deck
 * can start (PRD #1223 M7, from {@link authoringModes}).
 */
// The literal, not `NO_MODE_ID`: `voice_outcome_every_control_label_asks_for_its_own_row`
// reads this line to find the chip's label. `lib/newAgentDraft.ts` keeps the two equal.
const NO_MODE = { id: "none", label: "No mode" } as const;

type ModeId = typeof NO_MODE.id | AuthoringKind | ReturnType<typeof orchestrationModeId>;

/** Why the dialog cannot be closed during a start (PRD #1223 audit F5). */
export const STARTING_CLOSE_BLOCKED = "Waiting for the deck to answer the start. The dialog can be closed once it has.";

/*
  Issue #1247 — what a reopened dialog says about the form it put back. Each
  names what was restored or why a saved choice was not, because a restore
  that silently dropped a directory would read as the draft having been lost.
*/
/** Something the user chose or typed was put back. */
export const DRAFT_RESTORED = "Restored what was entered when this form was last closed. Discard clears it.";
/** The saved directory could not be listed again on its deck. */
export const DRAFT_DIRECTORY_GONE = "The directory chosen last time could not be listed on this deck any more, so it was not chosen again.";
/** The saved Mode chip is not offered on the restored form. */
export const DRAFT_MODE_GONE = "The Mode chosen last time is not offered on this form any more, so the form is back to No mode.";
/** The form was opened for a deck other than the saved one, which wins. */
export const draftOtherDeck = (savedDeck: string) => `This form was opened for another deck, so the directory chosen on ${savedDeck} last time was not restored.`;
/** The saved deck cannot take a new agent now, or has left the fleet. */
export const draftDeckGone = (savedDeck: string, hadDirectory: boolean) => `${savedDeck}, the deck chosen last time, cannot take a new agent now, so ${hadDirectory ? "it and its directory were" : "it was"} not restored.`;

/*
  Issue #1263 — the deck field by voice, in the dialog's own words. Rust
  resolves the spoken deck against the observed fleet and offers the model
  only the decks the field can choose, so each of these answers a fleet or a
  dialog that changed during the round trip.
*/
/** A start is in flight, when the deck field is disabled. */
export const DECK_CHANGE_IN_FLIGHT = "A start is under way, so the deck was not changed.";
/** The deck is not in the field's list any more. */
export const DECK_NOT_LISTED = "That deck is not in the New agent dialog's deck list any more, so the deck was not changed.";
/** The deck is listed, disabled. */
export const DECK_CANNOT_TAKE_AGENT = "That deck cannot take a new agent now, so the deck was not changed.";
/** The dialog closed during the round trip (served by the overview). */
export const NO_DIALOG_FOR_DECK = "The New agent dialog is not open, so no deck was chosen.";
/** The dialog closed during the round trip, so there was nothing to discard. */
export const NO_DIALOG_TO_DISCARD = "The New agent dialog is not open, so nothing was discarded.";

/*
  PRD #1223 — the directory browser's refusals by voice, in the dialog's own
  words. Rust already refuses a directory row when no browser was declared, so
  each of these answers a browser that changed during the round trip.
*/
/** No browser to move: the dialog is closed, has no listing, or is starting. */
export const NO_DIRECTORY_BROWSER = "The New agent dialog is not showing a directory listing, so nothing was changed.";
/** The browser moved between the utterance and its answer. */
export const DIRECTORY_MOVED_ON = "The directory browser moved on while that was being worked out, so nothing was changed. Say it again.";
/**
 * The named child is not among the rows on screen any more — gone from the
 * listing, or hidden by a filter typed during the round trip.
 */
export const DIRECTORY_NOT_LISTED = "That directory is not on screen any more, so nothing was opened.";
/** `..` is not on screen. */
export const NO_PARENT_DIRECTORY = "This directory has no parent to go up to.";
/*
  PRD #1223 — the form's refusals by voice. Rust refuses a fill row when no
  live form was declared, so each of these answers a form that changed during
  the round trip.
*/
/** No live form: the dialog is closed, has no deck or directory chosen, or is starting. */
export const NO_NEW_AGENT_FORM = "The New agent form has no deck and directory chosen yet, so nothing was changed.";
/** The form moved to another deck or directory between the utterance and its answer. */
export const FORM_MOVED_ON = "The New agent form moved on while that was being worked out, so nothing was changed. Say it again.";
/** The chip is not in the Mode row any more. */
export const MODE_NOT_OFFERED = "That mode is not offered on this form any more, so the mode was not changed.";
/** The agent is not among the ones this deck offers any more. */
export const AGENT_TYPE_NOT_OFFERED = "That agent is not offered on this deck any more, so the Command was not changed.";
/** An orchestration is selected, so there is no Command field to fill. */
export const COMMAND_HIDDEN_BY_ORCHESTRATION = "An orchestration is selected and each of its roles runs its own command, so the Command was not changed.";
/** The words after "name it" were only punctuation. */
export const NO_NAME_HEARD = "No name was heard after that, so the Name was not changed.";

/*
  PRD #802 D5 — why a spoken start opened no confirmation. Each names what is
  missing, and each says nothing was started, because nothing was: a start
  that cannot happen is refused here rather than confirmed.
*/
/** The dialog is not open (it closed during the round trip). */
export const NO_NEW_AGENT_DIALOG = "The New agent dialog is not open, so nothing was started.";
/** No deck chosen yet. */
export const START_NEEDS_DECK = "Nothing was started: choose a deck first.";
/** A deck, but no directory chosen yet. */
export const START_NEEDS_DIRECTORY = "Nothing was started: choose a directory first — the agent needs one to start in.";
/** A start is already in flight or waiting for the deck to list it. */
export const START_IN_FLIGHT = "A start is already under way, so nothing else was started.";

/**
 * A spoken Name as the Name field takes it: the words after the marked
 * boundary, with the whitespace around them and the sentence punctuation a
 * transcriber ends an utterance with ("billing worker.") taken off. Nothing
 * inside the name is changed.
 */
export function spokenName(text: string | undefined): string {
  return (text ?? "").trim().replace(/[\s.,!?;:]+$/u, "").trim();
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * PRD #1223 M4/M5 — the desktop's New agent flow: the TUI's `Ctrl+n` with a
 * deck field in front of it, because the desktop drives several decks.
 *
 * ONE surface, every field mounted at once (PRD #1223, the voice-first
 * redesign). It used to be a three-step wizard, and a wizard resists voice:
 * each step's meaning depends on the one before it, so there is nothing to
 * address until the previous gate is passed. Here every field is its own
 * nameable control, and changing one re-derives only its dependents —
 * deck → listing and options; directory → orchestration chips; agent →
 * Command.
 *
 * 1. **Deck** — every deck in the fleet; the ones that cannot take a spawn are
 *    listed disabled with the reason the overview gives for them.
 * 2. **Directory** — that deck's filesystem, browsed one level per request with
 *    the TUI picker's keys, in a panel of its own. Browsing is the only way to
 *    choose one (PRD #1223 U1 removed the typed path), so a deck without the
 *    listing verb is disabled in the deck field with the crate's
 *    `newAgentReason`.
 * 3. **Form** — Mode, Name and Command, prefilled in the TUI's order and
 *    enabled once a directory is chosen.
 *    Mode offers a plain agent; one `Orch: <name>` chip per orchestration the
 *    directory's project defines on that deck (PRD #1223 M6); and, on a deck
 *    that can compose their seeds, the TUI's `schedule`, `schedule: issues` and
 *    `dispatcher` authoring agents (PRD #1223 M7) — whose blank Command
 *    resolves the way the TUI's does, because a blank one would start the
 *    deck's default shell.
 *
 * # An orchestration (PRD #1223 M6)
 *
 * The TUI's rules, against the chosen deck. While one is selected and the Name
 * is untouched, the Name is the next free `<basename>-orchestrator-N` over the
 * titles live on that deck; a Name equal to one of those titles refuses the
 * start; and Command is hidden, because each role runs the command its config
 * gives it — on the deck, which reads that config. The launch prepares with no
 * task and starts every role there, and the START role's pane is the one
 * opened afterwards, as the TUI focuses it.
 *
 * # The deck is captured once
 *
 * Choosing a deck captures its wire `deckId`, and every later request —
 * listing, options, orchestrations, start — carries that value. Nothing reads
 * the selected deck. A request refused because the deck has left the fleet
 * clears the directory panel and the form, puts focus back on the deck field
 * and shows the refusal there, rather than aiming the next request anywhere
 * else.
 *
 * # No path is built here
 *
 * Every path the flow sends is one the deck returned — a listing's `path`,
 * `parent`, or an entry's `path`. The
 * Name prefill is the last component of the deck's canonical path, which is a
 * label and is never sent back as a path.
 *
 * # After the start
 *
 * The pane is not opened on the start's reply: `paneAgentRetired` in `App.tsx`
 * closes a pane whose connected deck does not list its agent. The dialog waits
 * until the target deck's fleet entry lists `(deckId, agentId)`, bounded by
 * {@link NEW_AGENT_APPEAR_TIMEOUT_MS}, and hands the identity to `onAppeared`.
 */
export function NewAgentDialog({ runtime, initialDeckId, draft, onClose, onAppeared, onNotAppeared, appearTimeoutMs = NEW_AGENT_APPEAR_TIMEOUT_MS, closeRequest, voice }: NewAgentDialogProps) {
  const titleId = useId();
  const choices = useMemo(() => deckChoices(runtime.fleet), [runtime.fleet]);
  /** The draft this mount was opened with (#1247) — read once, by the open effect. */
  const savedDraft = useRef(draft);
  /**
   * The deck field's cursor — moved by `j`/`k`, and not a choice until Enter or a click.
   *
   * On open it is the deck the flow was opened FOR, when it names one — a deck
   * header's button, or a spoken "new agent on the build box" — and otherwise
   * the saved draft's deck (#1247): an explicit request outranks a draft, whose
   * directory is then not restored (the open effect says so).
   */
  const [highlight, setHighlight] = useState<string | undefined>(() => preselectedDeck(deckChoices(runtime.fleet), initialDeckId ?? draft?.deckId));
  /** What the open put back and what it could not (#1247); absent for a fresh form. */
  const [restoreNotes, setRestoreNotes] = useState<string[]>();
  const [deckNotice, setDeckNotice] = useState<string>();
  /** The deck the flow is about — captured once per choice of it, in the deck field. */
  const [deck, setDeck] = useState<DeckChoice>();
  /**
   * Where focus goes next, once that control can take it: the one-shot
   * replacement for the wizard's per-step focus effects. `browser` waits for
   * the listing it is about to show.
   */
  const [focusRequest, setFocusRequest] = useState<{ to: "deck" | "browser" | "form" }>();

  // -- directory panel ----------------------------------------------------------
  const [listing, setListing] = useState<Listing>();
  const [listingState, setListingState] = useState<"idle" | "loading" | "ready" | "unsupported" | "failed">("idle");
  const [listingError, setListingError] = useState<string>();
  const [cursor, setCursor] = useState(0);
  const [filter, setFilter] = useState("");
  /** Drops the reply of any listing request a later one has superseded. */
  const listingSeq = useRef(0);
  /** Issue #1240: list `.`-named directories too, on a deck that honours listing options. */
  const [showHidden, setShowHidden] = useState(false);
  /**
   * Issue #1240: the deck's answer to the filter, for a listing its cap
   * truncated — the same directory searched deck-side, before the cap. Shown
   * only while `path` and `filter` are still the ones on screen.
   */
  const [searched, setSearched] = useState<{ path: string; filter: string; listing: Listing }>();
  const [searchError, setSearchError] = useState<string>();
  /** Drops a search reply a later keystroke or listing has superseded. */
  const searchSeq = useRef(0);

  // -- form -----------------------------------------------------------------------
  const [target, setTarget] = useState<{ path: string; displayPath: string }>();
  const [options, setOptions] = useState<NewAgentOptions>();
  const [optionsError, setOptionsError] = useState<string>();
  const [modeChoice, setModeChoice] = useState<ModeId>(NO_MODE.id);
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const commandTouched = useRef(false);
  /** Drops an options reply for a directory the user has since left. */
  const optionsSeq = useRef(0);
  const [formError, setFormError] = useState<string>();
  /**
   * The roles a failed launch could not confirm are stopped (PRD #1223 audit
   * F6) — carried as data by the crate, shown before `formError`'s clamped copy.
   */
  const [formCleanup, setFormCleanup] = useState<readonly string[]>();
  /** The deck's orchestrations for the chosen directory (PRD #1223 M6). */
  const [orchestrations, setOrchestrations] = useState<NewAgentOrchestrations>();
  const [orchestrationsError, setOrchestrationsError] = useState<string>();
  /** Drops an orchestrations reply for a directory the user has since left. */
  const orchestrationsSeq = useRef(0);
  /** An orchestrations query is in flight — the Mode row may still grow. */
  const [orchestrationsPending, setOrchestrationsPending] = useState(false);
  /**
   * #1247 — the saved directory and Mode not yet replayed: set when the open
   * chooses the saved deck, cleared once the deck has answered for them, and
   * dropped by anything that supersedes the restore (another deck, the user
   * browsing, the deck leaving). While it is set, a close keeps it, so a
   * dialog closed again before the deck answered still has its draft.
   */
  const resumePending = useRef<Resume | undefined>(undefined);
  /**
   * #1247 — the saved Mode chip, waiting for the Mode row to offer it: the
   * authoring chips come with the deck's options and the orchestration chips
   * with the directory's orchestrations, so the chip may appear a reply after
   * the directory does. Chosen as a click chooses it once it is offered, and
   * dropped with a note once every answer is in and it is not.
   */
  const pendingMode = useRef<string | undefined>(undefined);
  /** A human edited Name — a generated default may replace a generated default, never an edit. */
  const nameTouched = useRef(false);
  /**
   * Whether each path the deck listed holds a project, from the listings seen
   * so far — the marker that decides whether the deck is asked for
   * orchestrations at all. A directory reached another way (by going up) has
   * no entry here and is asked, and the deck's `not_project` answer covers it.
   *
   * Keyed by path alone, so it is DECK-SCOPED data and `clearDeckDependents`
   * drops it: two decks can expose the same path with different project status
   * (a remote deck's `/work` and a local one's), and keeping the previous
   * deck's `false` would suppress the orchestrations query on the new deck and
   * silently remove mode choices the user does have (Qodo on PR #1235). The
   * cost of dropping it is one extra query on a path revisited after a deck
   * switch; the cost of keeping it is a wrong answer.
   */
  const projectMarks = useRef(new Map<string, boolean>());
  /**
   * This mount of the dialog, for the voice surface to tell a pending answer's
   * dialog from the one on screen now (see `NewAgentVoice.instance`). A ref,
   * so reopening the dialog — a fresh mount — mints a fresh one, and a
   * re-render never does.
   */
  const instanceId = useRef(`new-agent-${Math.random().toString(36).slice(2)}-${Date.now()}`);
  const [phase, setPhase] = useState<"idle" | "starting" | "waiting">("idle");
  const [awaiting, setAwaiting] = useState<{ deckId: string; agentId: string; deckName: string; agentName: string }>();

  const deckListRef = useRef<HTMLUListElement>(null);
  const directoryListRef = useRef<HTMLUListElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const commandRef = useRef<HTMLInputElement>(null);

  /**
   * Everything that hangs off the chosen deck, dropped: the directory panel,
   * the chosen directory, the deck's options and orchestrations, the project
   * markers its listings produced, and every reply still in flight for them. `keepEdits` keeps a Name or Command the
   * user typed — a voluntary change of deck — and otherwise the form is
   * cleared as well.
   */
  const clearDeckDependents = useCallback((keepEdits: boolean) => {
    listingSeq.current += 1;
    optionsSeq.current += 1;
    orchestrationsSeq.current += 1;
    projectMarks.current.clear();
    resumePending.current = undefined;
    pendingMode.current = undefined;
    setOrchestrationsPending(false);
    setListing(undefined);
    setListingState("idle");
    setListingError(undefined);
    setFilter("");
    searchSeq.current += 1;
    setSearched(undefined);
    setSearchError(undefined);
    setCursor(0);
    setTarget(undefined);
    setOptions(undefined);
    setOptionsError(undefined);
    setOrchestrations(undefined);
    setOrchestrationsError(undefined);
    setModeChoice(NO_MODE.id);
    setFormError(undefined);
    setFormCleanup(undefined);
    if (!keepEdits || !nameTouched.current) {
      setName("");
      nameTouched.current = false;
    }
    if (!keepEdits || !commandTouched.current) {
      setCommand("");
      commandTouched.current = false;
    }
  }, []);

  /**
   * The chosen deck is no longer one this app observes. What the wizard did by
   * going back to its deck step, without the navigation: the directory panel
   * and the form are cleared, focus goes back to the deck field, and the
   * refusal is shown there. The highlight is recomputed over the fleet as it is
   * now — never carried over to another deck — and nothing is chosen for the
   * user.
   */
  const deckGone = useCallback((message: string) => {
    clearDeckDependents(false);
    setDeck(undefined);
    setDeckNotice(message);
    setPhase("idle");
    setAwaiting(undefined);
    setHighlight(preselectedDeck(deckChoices(runtime.fleet)));
    setFocusRequest({ to: "deck" });
  }, [clearDeckDependents, runtime.fleet]);

  /**
   * PRD #1223 audit F5 — whether this dialog is still mounted. A start's reply
   * can outlive the dialog (the overview can drop it for its own reasons), and
   * the failure handler below must then touch nothing: not local state, which
   * is gone, and above all not the runtime's global error, which is by then the
   * ONLY copy of the failure — and, for a launch whose rollback could not
   * confirm every stop, the only place the user can learn that roles may
   * still be running.
   */
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  /**
   * Every way out of the dialog — the header's close button, Esc, a backdrop
   * click, the directory panel's `q` and, through `closeRequest`, voice's
   * `close` (PRD #1223 U5), all of which keep the form as a draft (#1247), and
   * Discard, which does not — and none of them works while
   * a start is in flight (PRD #1223 audit F5). Closing then would unmount the
   * one place a failure is explained, while the action itself carries on.
   *
   * The wait is bounded instead, with ONE exception (audit W6 — this comment
   * said "every deck call a start makes is bounded", which audit V1 had
   * deliberately made false): each role start, each rollback stop, and the two
   * reads an orchestration launch makes before it prepares anything get
   * `DECK_REPLY_TIMEOUT`, but `PrepareWorkflow` gets no client-side deadline at
   * all, because dropping that future cannot stop the publish the deck has
   * already begun. So a deck that takes the connection and never answers a
   * PREPARATION does hold this dialog open — which is the trade audit V1 made,
   * against a preparation reported as timed out that publishes afterwards over
   * a retry's context. A failure that lands after the dialog is gone for some
   * other reason is reported on the runtime's global toast, which both screens
   * render (audit W2).
   *
   * Once the deck has answered — the "waiting for the fleet" phase included —
   * closing works as before.
   */
  const starting = phase === "starting";
  /**
   * #1247 — the form as a draft to keep: the CHOICES on it, never the deck's
   * answers (see `lib/newAgentDraft.ts`). A restore still in progress is kept
   * as it was saved, so closing a reopened dialog before the deck answered
   * loses nothing. Nothing is kept once a start has been accepted — the
   * "waiting for the deck to list it" phase — because that form has been
   * started, and reopening it would invite starting it twice.
   */
  const snapshot = (): NewAgentDraft | undefined => {
    if (phase !== "idle") return undefined;
    const resume = resumePending.current;
    const directory = target ?? resume?.directory;
    const browsing = listing?.path ?? resume?.browsing;
    const kept: NewAgentDraft = {
      ...(deck ? { deckId: deck.deckId, deckName: deck.name } : {}),
      ...(browsing !== undefined ? { browsing } : {}),
      ...(directory ? { directory: { path: directory.path, displayPath: directory.displayPath } } : {}),
      mode: resume && !target ? resume.mode : pendingMode.current ?? mode,
      name,
      nameTouched: nameTouched.current,
      command,
      commandTouched: commandTouched.current,
    };
    return draftWorthKeeping(kept) ? kept : undefined;
  };
  /**
   * Close, or answer why not. Every route calls this; only voice reads the
   * answer. It KEEPS the form as a draft (#1247): Esc, a stray backdrop click
   * or a steered `close` used to throw a filled form away.
   */
  const requestClose = (): string | undefined => {
    if (starting) return STARTING_CLOSE_BLOCKED;
    onClose(snapshot());
    return undefined;
  };
  /**
   * The Discard button and voice's `discard_new_agent` (#1247): close, and keep
   * nothing. Blocked during a start for `requestClose`'s reason.
   */
  const requestDiscard = (): string | undefined => {
    if (starting) return STARTING_CLOSE_BLOCKED;
    onClose(undefined);
    return undefined;
  };
  /* No dependency array: the slot holds this render's `requestClose`, which
     reads this render's `phase`. */
  useEffect(() => {
    if (!closeRequest) return;
    closeRequest.current = requestClose;
    return () => { closeRequest.current = undefined; };
  });

  /**
   * A start the deck refused. The runtime files a failed action under its
   * global error as well; a MOUNTED dialog says it here, beside the values, so
   * that copy is dropped. An unmounted one leaves it alone — see `mounted`.
   */
  const failStart = (cause: unknown) => {
    if (!mounted.current) return;
    runtime.clearError();
    const message = messageOf(cause);
    // PRD #1223 audit V3: the structured cleanup is read FIRST. Deck loss
    // clears the form and shows the refusal as prose beside the deck field and
    // nothing else, so classifying such a failure as deck loss would drop the
    // roles that may still be running —
    // and `isDeckGoneError` reads the start of the message for the same
    // reason, since a role name is interpolated into this sentence.
    const cleanup = cause instanceof LaunchCleanupError ? cause.unconfirmedStops : undefined;
    if (cleanup === undefined && isDeckGoneError(message)) {
      deckGone(message);
      return;
    }
    setFormError(message);
    setFormCleanup(cleanup);
    setPhase("idle");
  };

  /** Issue #1240: the deck's search of this directory for this filter, when that is what is on screen. */
  const searchedHere = searched !== undefined && listing !== undefined && filter !== "" && searched.path === listing.path && searched.filter === filter ? searched.listing : undefined;
  const rows = useMemo<DirectoryRow[]>(() => {
    if (!listing) return [];
    const up: DirectoryRow[] = listing.parent === undefined ? [] : [{ kind: "up", path: listing.parent }];
    return [...up, ...filterDirectoryEntries((searchedHere ?? listing).entries, filter).map((entry): DirectoryRow => ({ kind: "entry", entry }))];
  }, [filter, listing, searchedHere]);

  /*
    Issue #1240: what `loadListing` needs to know about the browser at the
    moment it asks — which decks honour listing options, and whether Show
    hidden is on — rewritten every render, and by `toggleHidden` directly so a
    reload it starts sees the new value before the next render does.
  */
  const browse = useRef({ showHidden, filter, supports: new Set<string>() });
  browse.current = { showHidden, filter, supports: new Set(choices.flatMap((choice) => (choice.listingOptions ? [choice.deckId] : []))) };
  /** Issue #1240: where a search that lands after a Show hidden reload should put the cursor. */
  const searchFocus = useRef<Focus | undefined>(undefined);

  /**
   * List `path` on the captured deck (its home when absent), then put the
   * cursor on `focusPath` — the directory just left, when going up — or else
   * on the first subdirectory rather than on `..`.
   *
   * `onFailure` says what a failure that is not deck loss does. `report`
   * shows it in the panel. `home` is the deck's configured default
   * directory's fallback (PRD #1223): the deck vetted that path when it
   * answered the options query, but it can vanish before the listing lands,
   * and a browser that opened on an error would be a worse start than the home
   * it replaces. `quiet` shows nothing and leaves the next request to the
   * caller — a draft's restore (#1247), which falls back to where a fresh
   * form starts and says why in its own words.
   *
   * The answer is what became of the request, for a caller that has more to
   * do once it lands; every other caller ignores it.
   */
  const loadListing = useCallback(async (deckId: string, path?: string, focusPath?: string | Focus, onFailure: "report" | "home" | "quiet" = "report", keepFilter = false): Promise<ListingOutcome> => {
    const seq = ++listingSeq.current;
    setListingError(undefined);
    setListingState((current) => (current === "ready" ? current : "loading"));
    try {
      const options = directoryListingOptions(browse.current.supports.has(deckId), browse.current.showHidden);
      const reply = await (options ? runtime.listDirectories(deckId, path, options) : runtime.listDirectories(deckId, path));
      if (seq !== listingSeq.current) return { kind: "stale" };
      if (reply.kind === "unsupported") {
        setListing(undefined);
        setListingState("unsupported");
        return { kind: "unsupported" };
      }
      for (const entry of reply.entries) projectMarks.current.set(entry.path, entry.isProject);
      setListing(reply);
      setListingState("ready");
      // Issue #1240: a Show hidden reload keeps the filter the user typed —
      // and, on a truncated listing, the deck search re-runs for it with the
      // new setting — so the cursor is placed among the rows it leaves.
      const kept = keepFilter ? browse.current.filter : "";
      if (!keepFilter) setFilter("");
      searchSeq.current += 1;
      setSearched(undefined);
      setSearchError(undefined);
      const focus = typeof focusPath === "string" ? { path: focusPath } : focusPath;
      searchFocus.current = keepFilter ? focus : undefined;
      const shown = filterDirectoryEntries(reply.entries, kept);
      const offset = reply.parent === undefined ? 0 : 1;
      const focused = focusIndex(shown, focus);
      setCursor(focused >= 0 ? focused + offset : shown.length > 0 ? offset : 0);
      return { kind: "listed", listing: reply };
    } catch (cause) {
      if (seq !== listingSeq.current) return { kind: "stale" };
      const message = messageOf(cause);
      if (isDeckGoneError(message)) {
        deckGone(message);
        return { kind: "stale" };
      }
      if (onFailure === "home" && path !== undefined) {
        void loadListing(deckId);
        return { kind: "failed" };
      }
      if (onFailure === "quiet") return { kind: "failed" };
      setListingError(message);
      setListingState((current) => (current === "ready" ? current : "failed"));
      return { kind: "failed" };
    }
  }, [deckGone, runtime]);

  /**
   * Choosing a deck in the deck field: capture its wire id — once, here — and
   * ask THAT deck for its new-agent options and then its first listing. Choosing
   * the deck already chosen changes nothing and only moves on to the browser,
   * so Enter on a preselected deck does what the wizard's one keystroke did.
   *
   * **The listing waits for the options, deliberately** (PRD #1223): the
   * options carry the deck's configured `defaultDir`, and the browser opens
   * THERE when the deck names one — the folder most agents on that deck are
   * started under — and in the daemon user's home otherwise. Listing home in
   * parallel and then jumping would flash one directory and land on another.
   * `..` still walks above the default; it is a starting point, not a root.
   * An options query that fails for any reason but a lost deck still lists
   * home, so the browser never waits on a setting.
   *
   * `resume` is a saved draft's directory and Mode (#1247), replayed through
   * {@link resumeDraft} once the options have answered, in place of the
   * default directory.
   */
  const chooseDeck = (choice: DeckChoice | undefined, resume?: Resume) => {
    if (!choice || choice.reason !== undefined || phase !== "idle") return;
    setHighlight(choice.deckId);
    setFocusRequest({ to: "browser" });
    if (deck?.deckId === choice.deckId) return;
    clearDeckDependents(true);
    setDeck(choice);
    setDeckNotice(undefined);
    setListingState("loading");
    const deckId = choice.deckId;
    const seq = ++optionsSeq.current;
    resumePending.current = resume;
    void (async () => {
      let startAt: string | undefined;
      try {
        const answer = await runtime.newAgentOptions(deckId);
        if (seq !== optionsSeq.current) return;
        setOptions(answer);
        if (answer.kind === "deck") startAt = answer.defaultDir;
        if (!commandTouched.current) {
          setCommand(answer.kind === "deck" ? seedCommand(answer.defaultCommand, answer.lastCommand) : seedCommand(undefined, answer.lastCommand));
        }
      } catch (cause) {
        if (seq !== optionsSeq.current) return;
        const message = messageOf(cause);
        if (isDeckGoneError(message)) {
          deckGone(message);
          return;
        }
        setOptionsError(message);
      }
      if (resume) void resumeDraft(deckId, resume, startAt);
      else void loadListing(deckId, startAt, undefined, startAt !== undefined ? "home" : "report");
    })();
  };

  /**
   * #1247 — a saved draft's directory, replayed against the deck as it is NOW.
   *
   * The chosen directory (or, with none chosen, where the browser was) is
   * listed afresh: a directory the deck still lists is chosen again exactly as
   * Use this directory chooses it, by the path the deck returns now, which
   * re-asks for its orchestrations; one it cannot list is dropped with a note
   * and the browser opens where a fresh form would. Its saved Mode waits in
   * {@link pendingMode} for the Mode row to offer it.
   *
   * **Anything the user does meanwhile wins.** Choosing another deck, or the
   * deck leaving, clears {@link resumePending}; browsing makes this listing
   * stale. Either way nothing here is applied over it.
   *
   * No path is built here either: every path sent is one the deck returned
   * when the draft was made.
   */
  const resumeDraft = async (deckId: string, resume: Resume, startAt?: string) => {
    const wanted = resume.directory?.path ?? resume.browsing;
    const fresh = () => void loadListing(deckId, startAt, undefined, startAt !== undefined ? "home" : "report");
    if (wanted === undefined) {
      resumePending.current = undefined;
      fresh();
      return;
    }
    const outcome = await loadListing(deckId, wanted, undefined, "quiet");
    if (resumePending.current !== resume) return;
    resumePending.current = undefined;
    if (outcome.kind === "failed" || outcome.kind === "unsupported") {
      // Everything that hung off the directory goes with it, and each loss is
      // named: its Mode too, since a Mode is chosen for a directory.
      const dropped = resume.directory ? [DRAFT_DIRECTORY_GONE, ...(resume.mode !== NO_MODE.id ? [DRAFT_MODE_GONE] : [])] : [];
      if (dropped.length > 0) setRestoreNotes((notes) => [...(notes ?? []), ...dropped]);
      // A deck that cannot list at all leaves the panel saying so, as a fresh
      // form on it does; listing its default directory would say the same twice.
      if (outcome.kind === "failed") fresh();
      return;
    }
    if (outcome.kind === "listed" && resume.directory) {
      confirmDirectory(outcome.listing.path, outcome.listing.displayPath, { deckId, mode: resume.mode });
    }
  };

  /**
   * The deck field's preselection — the only eligible deck, or the one whose
   * header opened the flow — is CHOSEN on open, so its listing loads at once
   * and there is no Next to press. Otherwise focus starts on the deck field,
   * the first control left unsatisfied.
   *
   * # A saved draft (#1247)
   *
   * Restoring is REPLAYING, not copying. A typed Name and an edited Command
   * are put back as they were — they are the user's own text, and no answer
   * from the deck can invalidate them. Everything else is re-derived: the deck
   * is chosen again only while it can still take a spawn, which re-asks it for
   * its options (so an untouched Command is seeded from what the deck says
   * NOW, and the agent registry is the current one); its directory is
   * re-listed and re-chosen only if the deck still lists it
   * ({@link resumeDraft}); and its Mode chip is chosen only once the fresh
   * answers offer it, which also regenerates an untouched orchestration Name
   * against the run titles live now. The project markers are not saved at
   * all: the fresh listing produces them, and an unmarked directory is asked
   * for its orchestrations anyway.
   *
   * A deck the flow was opened FOR — `initialDeckId` — outranks the draft's.
   * On another deck the draft keeps only its Name and Command, exactly as a
   * change of deck in the field does, and a note says what was not restored.
   */
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current) return;
    opened.current = true;
    const preselected = choices.find((choice) => choice.deckId === highlight);
    const eligible = preselected !== undefined && preselected.reason === undefined;
    const saved = savedDraft.current;
    let resume: Resume | undefined;
    if (saved) {
      if (saved.nameTouched) {
        nameTouched.current = true;
        setName(saved.name);
      }
      if (saved.commandTouched) {
        commandTouched.current = true;
        setCommand(saved.command);
      }
      const notes = draftHasEdits(saved) ? [DRAFT_RESTORED] : [];
      if (saved.deckId !== undefined) {
        const savedName = displayText(saved.deckName ?? "The deck", DISPLAY_LIMITS.name);
        if (eligible && preselected.deckId === saved.deckId) {
          resume = {
            ...(saved.browsing !== undefined ? { browsing: saved.browsing } : {}),
            ...(saved.directory ? { directory: saved.directory } : {}),
            mode: saved.mode,
          };
        } else if (initialDeckId !== undefined && initialDeckId !== saved.deckId) {
          if (saved.directory) notes.push(draftOtherDeck(savedName));
        } else {
          notes.push(draftDeckGone(savedName, saved.directory !== undefined));
        }
      }
      if (notes.length > 0) setRestoreNotes(notes);
    }
    if (eligible) chooseDeck(preselected, resume);
    else setFocusRequest({ to: "deck" });
  });

  /**
   * A directory the deck returned, chosen: the Name follows it while untouched,
   * the Mode goes back to No mode as every fresh TUI form does, and the deck is
   * asked for its orchestrations. Agent and Command stay — they hang off the
   * deck, not the directory.
   */
  const confirmDirectory = (path: string, displayPath: string, restoring?: { deckId: string; mode: string }) => {
    // A restore (#1247) runs from the reply to a request this render did not
    // make, so it names the deck it captured rather than reading `deck`.
    const deckId = restoring?.deckId ?? deck?.deckId;
    if (deckId === undefined || phase !== "idle") return;
    setTarget({ path, displayPath });
    if (!nameTouched.current) setName(directoryLabel(path));
    setModeChoice(NO_MODE.id);
    pendingMode.current = restoring && restoring.mode !== NO_MODE.id ? restoring.mode : undefined;
    setOrchestrations(undefined);
    setOrchestrationsError(undefined);
    setFormError(undefined);
    setFormCleanup(undefined);
    setFocusRequest({ to: "form" });
    // PRD #1223 M6: the deck's orchestrations for this directory — asked unless
    // a listing already marked it as no project. The deck answers `not_project`
    // for an ordinary directory, so asking about an unmarked one is safe.
    const orchestrationsSeqNow = ++orchestrationsSeq.current;
    const queryOrchestrations = runtime.newAgentOrchestrations;
    const asking = queryOrchestrations !== undefined && projectMarks.current.get(path) !== false;
    setOrchestrationsPending(asking);
    if (asking) {
      void (async () => {
        try {
          const answer = await queryOrchestrations(deckId, path);
          if (orchestrationsSeqNow !== orchestrationsSeq.current) return;
          setOrchestrations(answer);
          setOrchestrationsPending(false);
        } catch (cause) {
          if (orchestrationsSeqNow !== orchestrationsSeq.current) return;
          setOrchestrationsPending(false);
          const message = messageOf(cause);
          if (isDeckGoneError(message)) deckGone(message);
          else setOrchestrationsError(message);
        }
      })();
    }
  };

  const confirmCurrent = () => {
    if (listing) confirmDirectory(listing.path, listing.displayPath);
  };

  const goUp = () => {
    if (deck && listing?.parent !== undefined) void loadListing(deck.deckId, listing.parent, listing.path);
  };

  /**
   * Issue #1240 — Show hidden, on a deck that honours listing options: the
   * directory on screen is listed again with `.`-named directories included
   * (or left out). The filter stays, a deck search it had made is made again
   * with the new setting, and the cursor stays on the row it was on — by
   * name as well as path — when that row is still listed.
   */
  const toggleHidden = (next: boolean) => {
    if (!deck?.listingOptions || busy) return;
    setShowHidden(next);
    browse.current = { ...browse.current, showHidden: next };
    const current = rows[cursor];
    const focus = current?.kind === "entry" ? { path: current.entry.path, name: current.entry.displayName } : undefined;
    if (listing) void loadListing(deck.deckId, listing.path, focus, "report", true);
  };

  /*
    Issue #1240 — past the cap. When the deck's cap truncated the listing on
    screen, the filter cannot find a directory the listing never received, so
    once typing pauses the deck is asked to search the same directory for it:
    the deck applies the filter BEFORE its cap, and its answer replaces the
    listing's entries while that filter and that directory stay on screen. A
    listing that was not truncated is complete, so the filter keeps narrowing
    it here with no round trip. A deck without listing options is never asked.
  */
  /*
    Read through a ref rather than depended on: a fleet snapshot can replace
    both every 150 ms on a busy deck, and a dependency on either would restart
    the debounce each time, so the search might never be sent.
  */
  const searchDeps = useRef({ runtime, deckGone });
  searchDeps.current = { runtime, deckGone };
  useEffect(() => {
    if (!deck?.listingOptions || !listing?.truncated || filter === "") {
      // Nothing to search for (the filter was cleared, say): a request still
      // in flight answers nothing on screen, including its failure.
      searchSeq.current += 1;
      setSearchError(undefined);
      return;
    }
    if (searched && searched.path === listing.path && searched.filter === filter) return;
    const seq = ++searchSeq.current;
    const deckId = deck.deckId;
    const path = listing.path;
    const wanted = filter;
    const timer = setTimeout(() => {
      void (async () => {
        try {
          const options = directoryListingOptions(true, browse.current.showHidden, wanted);
          const reply = await searchDeps.current.runtime.listDirectories(deckId, path, options);
          if (seq !== searchSeq.current || reply.kind !== "listing") return;
          for (const entry of reply.entries) projectMarks.current.set(entry.path, entry.isProject);
          setSearchError(undefined);
          setSearched({ path, filter: wanted, listing: reply });
          const offset = listing.parent === undefined ? 0 : 1;
          const shown = filterDirectoryEntries(reply.entries, wanted);
          const focused = focusIndex(shown, searchFocus.current);
          searchFocus.current = undefined;
          // On the row a Show hidden reload was on, or else where the cursor
          // was, kept inside the rows this answer leaves.
          setCursor((current) => (focused >= 0 ? focused + offset : Math.max(0, Math.min(current, shown.length + offset - 1))));
        } catch (cause) {
          if (seq !== searchSeq.current) return;
          const message = messageOf(cause);
          if (isDeckGoneError(message)) searchDeps.current.deckGone(message);
          else setSearchError(message);
        }
      })();
    }, DECK_SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [deck, filter, listing, searched]);
  /** A search is on its way: the filter is on a truncated listing and the deck has not answered for it yet. */
  const searching = deck?.listingOptions === true && listing?.truncated === true && filter !== "" && searchedHere === undefined && searchError === undefined;

  /**
   * PRD #1223 — the directory browser by voice. Each move calls the function
   * the browser's own control calls (a click on a row, the `..` row / `h`, the
   * Use this directory button / Space) and nothing else, so voice and the
   * keyboard cannot come to disagree about what a move does.
   *
   * **The browser can move during the round trip**, so each first re-checks
   * that it is still showing what the utterance was judged against — the deck
   * and the listing `path` in `target.declaredDirectories` — and refuses in
   * the dialog's own words when it is not, rather than acting on a listing the
   * user has since left. A start in flight refuses too, exactly as every
   * control inside the dialog is disabled then. The deck and the path are not
   * the whole of what is on screen — the filter is the rest — so `open_dir`
   * also requires its target among the rows the filter shows NOW.
   */
  const browserMovedOn = (target: VoiceDispatchTarget): string | undefined => {
    if (!deck || !listing || phase !== "idle") return NO_DIRECTORY_BROWSER;
    const declared = target.declaredDirectories;
    if (!declared || declared.deckId !== deck.deckId || declared.path !== listing.path) return DIRECTORY_MOVED_ON;
    return undefined;
  };
  const voiceOpenDirectory = (target: VoiceDispatchTarget): string | undefined => {
    const refused = browserMovedOn(target);
    if (refused !== undefined || !deck || !listing) return refused;
    // Against the ROWS on screen, after the filter — not the unfiltered
    // listing. The deck and the listing path can both be unchanged while the
    // filter moved during the round trip ("doc" to "bill"), and a spoken name
    // means a directory the user can see, so a child the filter now hides is
    // refused rather than opened from behind it.
    const index = rows.findIndex((row) => row.kind === "entry" && row.entry.path === target.directoryPath);
    const row = rows[index];
    if (row?.kind !== "entry") return DIRECTORY_NOT_LISTED;
    // A click on the row: the cursor lands on it, then the deck lists it.
    setCursor(index);
    void loadListing(deck.deckId, row.entry.path);
    return undefined;
  };
  const voiceGoToParent = (target: VoiceDispatchTarget): string | undefined => {
    const refused = browserMovedOn(target);
    if (refused !== undefined) return refused;
    if (listing?.parent === undefined) return NO_PARENT_DIRECTORY;
    goUp();
    return undefined;
  };
  const voiceUseThisDirectory = (target: VoiceDispatchTarget): string | undefined => {
    const refused = browserMovedOn(target);
    if (refused !== undefined) return refused;
    confirmCurrent();
    return undefined;
  };
  /**
   * PRD #1223 — the rest of the form by voice: Mode, the agent and Name. Mode
   * and Name call what the control's own click or keystroke calls
   * (`selectMode`, the Name input's setter) and nothing else, so a spoken Name
   * is an edit exactly as a typed one is. The agent has no control of its own
   * any more — the Agent picker was removed, because every `default_command`
   * is the bare binary name and the picker saved one word of typing while its
   * `auto` meant nothing and its label went stale against an edited Command —
   * so "use claude" sets Command to that agent's default command
   * ({@link commandFromAgent}), resolved against the deck's own registry (or
   * this app's fallback copy for a deck that reports none). Voice has no other
   * way to choose what the agent runs, since Command is never dictated.
   *
   * Each first re-checks that the form is still the one the utterance was
   * judged against — its deck and its chosen directory — for the browser's
   * reason, and refuses in the dialog's words when it is not.
   *
   * Command has no move: it is the field that executes, and it stays typed.
   */
  const formMovedOn = (dispatch: VoiceDispatchTarget): string | undefined => {
    if (!deck || !target || phase !== "idle") return NO_NEW_AGENT_FORM;
    const declared = dispatch.declaredForm;
    if (!declared || declared.deckId !== deck.deckId || declared.path !== target.path) return FORM_MOVED_ON;
    return undefined;
  };
  const voiceChooseMode = (dispatch: VoiceDispatchTarget): string | undefined => {
    const refused = formMovedOn(dispatch);
    if (refused !== undefined) return refused;
    const chip = modes.find((candidate) => candidate.id === dispatch.modeId);
    if (!chip) return MODE_NOT_OFFERED;
    selectMode(chip.id);
    return undefined;
  };
  const voiceChooseAgentType = (dispatch: VoiceDispatchTarget): string | undefined => {
    const refused = formMovedOn(dispatch);
    if (refused !== undefined) return refused;
    if (selectedOrchestration) return COMMAND_HIDDEN_BY_ORCHESTRATION;
    const agent = agents.find((candidate) => candidate.id === dispatch.agentTypeId);
    if (!agent) return AGENT_TYPE_NOT_OFFERED;
    commandFromAgent(agent);
    return undefined;
  };
  const voiceNameNewAgent = (dispatch: VoiceDispatchTarget): string | undefined => {
    const refused = formMovedOn(dispatch);
    if (refused !== undefined) return refused;
    const spoken = spokenName(dispatch.text);
    if (!spoken) return NO_NAME_HEARD;
    nameTouched.current = true;
    setName(spoken);
    return undefined;
  };
  /**
   * Issue #1263 — the deck field by voice: "deck build box", "use the local
   * deck". It calls {@link chooseDeck}, the function a click on the deck's row
   * calls, and nothing else — so a different deck clears the chosen directory,
   * its listing, the deck's options and its orchestrations and keeps only a
   * typed Name or Command, exactly as the click does, and the deck already
   * chosen changes nothing.
   *
   * **Callable whenever the dialog is open** (`requires = ["new_agent_dialog"]`),
   * unlike the fill rows, because choosing a deck is how the form BECOMES live.
   * The deck arrives as the row's `deck_ref`, resolved Rust-side against the
   * observed fleet, and is looked up in the field's list as it is NOW: one that
   * has left it, or can no longer take a spawn, is refused in the dialog's
   * words rather than chosen. A deck changed by hand during the round trip is
   * replaced by the one the user named, as a second click would replace it;
   * the voice surface refuses the answer outright if the dialog's FORM became
   * or stopped being live meanwhile (`sameNewAgentDeclaration`), which is what
   * protects a directory chosen during the round trip.
   */
  const voiceChooseDeck = (dispatch: VoiceDispatchTarget): string | undefined => {
    if (phase !== "idle") return DECK_CHANGE_IN_FLIGHT;
    const choice = choices.find((candidate) => candidate.deckId === dispatch.preselectDeckId);
    if (!choice) return DECK_NOT_LISTED;
    if (choice.reason !== undefined) return DECK_CANNOT_TAKE_AGENT;
    chooseDeck(choice);
    return undefined;
  };
  /** Whether the form's fields are live — what a click on one of them needs. */
  const formLive = deck !== undefined && target !== undefined && phase === "idle";

  /*
    The slot, rewritten every render — no dependency array, for `closeRequest`'s
    reason: the moves close over this render's listing and phase. The
    declaration is present only while there is something on screen to name: a
    deck chosen, a listing landed, and no start in flight (every control in the
    browser is disabled then). Its entries are the ROWS on screen, after the
    filter, because a spoken name means one the user can see.
  */
  useEffect(() => {
    if (!voice) return;
    voice.current = {
      directories: deck && listing && phase === "idle"
        ? {
          deckId: deck.deckId,
          path: listing.path,
          hasParent: listing.parent !== undefined,
          entries: rows.flatMap((row) => (row.kind === "entry" ? [{ name: row.entry.displayName, path: row.entry.path }] : [])),
        }
        : undefined,
      openDirectory: voiceOpenDirectory,
      goToParentDirectory: voiceGoToParent,
      useThisDirectory: voiceUseThisDirectory,
      /* PRD #1223 — present while mounted; its `form` only while the fields
         are live, carrying the chips and picker entries AS OFFERED on this
         render — a disabled namesake orchestration chip is not among them,
         since a click cannot choose one either. */
      newAgent: {
        form: formLive && deck && target
          ? {
            deckId: deck.deckId,
            path: target.path,
            modes: modes.map(({ id, label }) => ({ id, label })),
            agentTypes: agents.map((agent) => ({ id: agent.id, label: agent.displayName })),
            /* The authoring chips this form withholds — for a deck that cannot
               compose them, or `schedule: issues` with its flag off — so a
               spoken one is refused BY NAME instead of becoming the nearest
               offered chip. Resolvable never; see `withheld_modes` in Rust. */
            withheldModes: AUTHORING_MODES.filter((candidate) => !authoring.offered.some((offered) => offered.kind === candidate.kind)).map(({ kind, label }) => ({ id: kind, label })),
          }
          : undefined,
      },
      chooseNewAgentDeck: voiceChooseDeck,
      chooseNewAgentMode: voiceChooseMode,
      chooseNewAgentType: voiceChooseAgentType,
      nameNewAgent: voiceNameNewAgent,
      startNewAgent: voiceStart,
      discardNewAgent: requestDiscard,
      instance: instanceId.current,
    };
    return () => { voice.current = undefined; };
  });

  /** The TUI picker's Enter / `l` / Right: a directory with no subdirectories is confirmed, otherwise the row under the cursor is entered. */
  const openRow = () => {
    if (!deck || !listing) return;
    if (listing.entries.length === 0) {
      confirmCurrent();
      return;
    }
    const row = rows[cursor];
    if (!row) return;
    if (row.kind === "up") goUp();
    else void loadListing(deck.deckId, row.entry.path);
  };

  const agents: NewAgentOption[] = options?.kind === "deck" ? options.agents : options?.kind === "unsupported" ? options.desktopAgents : [];
  const authoring = authoringModes(options);
  const orchestrationOffer = orchestrationModes(orchestrations);
  const orchestrationChips = orchestrationOffer.offered.map((orchestration) => ({
    id: orchestrationModeId(orchestration.name),
    label: `Orch: ${displayText(orchestration.displayName, DISPLAY_LIMITS.name)}`,
    orchestration,
  }));
  const modes: { id: ModeId; label: string }[] = [
    NO_MODE,
    ...orchestrationChips.map(({ id, label }) => ({ id, label })),
    ...authoring.offered.map((mode) => ({ id: mode.kind, label: mode.label })),
  ];
  /**
   * PRD #1223 audit F2 — the project's namesake orchestrations, shown after
   * the ones that can be chosen and disabled with the reason. They are not in
   * `modes`, so neither a click nor the arrow keys can select one, and no
   * ambiguous name can reach the launch. One reason line per shared name.
   */
  const ambiguousChips = (orchestrationOffer.ambiguous ?? []).map((orchestration, index) => ({
    key: `ambiguous-${index}`,
    label: `Orch: ${displayText(orchestration.displayName, DISPLAY_LIMITS.name)}`,
    reason: ambiguousOrchestrationReason(orchestration.displayName),
  }));
  const ambiguousReasons = [...new Set(ambiguousChips.map((chip) => chip.reason))];
  /** Where the disabled namesakes sit in the Mode row: after the orchestrations that can be chosen. */
  const orchestrationEnd = 1 + orchestrationChips.length;
  /** The chip in force — `No mode` whenever the chosen one is not (or no longer) offered. */
  const mode: ModeId = modes.some((candidate) => candidate.id === modeChoice) ? modeChoice : NO_MODE.id;
  const selectedOrchestration: DaemonOrchestration | undefined = orchestrationChips.find((chip) => chip.id === mode)?.orchestration;
  const authoringKind: AuthoringKind | undefined = AUTHORING_MODES.find((candidate) => candidate.kind === mode)?.kind;
  const defaultCommand = options?.kind === "deck" ? options.defaultCommand : undefined;

  // PRD #1223 M6 — the TUI's Name rules, against the CHOSEN deck's own fleet
  // entry: never another deck's orchestrations, and never the selected deck's.
  const liveTitles = useMemo(() => (deck ? liveOrchestrationTitles(runtime.fleet, deck.deckId) : []), [deck, runtime.fleet]);
  const liveDirectories = useMemo(() => (deck ? liveOrchestrationDirectories(runtime.fleet, deck.deckId) : []), [deck, runtime.fleet]);
  const runTitle = selectedOrchestration ? orchestrationRunTitle(name, selectedOrchestration.name) : undefined;
  const titleTaken = runTitle !== undefined && liveTitles.includes(runTitle);
  const sameDirectory = selectedOrchestration !== undefined && orchestrations?.kind === "project" && liveDirectories.includes(orchestrations.path);

  /**
   * The TUI's `resuggest_name_for_selection`: landing on an orchestration
   * suggests the next free `<basename>-orchestrator-N`, and landing anywhere
   * else restores the directory's basename — both only while the Name is
   * untouched.
   */
  const selectMode = (id: ModeId) => {
    // A chip chosen by the user supersedes a draft's chip still waiting (#1247).
    pendingMode.current = undefined;
    setModeChoice(id);
    if (nameTouched.current || !target) return;
    const basename = directoryLabel(target.path);
    setName(orchestrationChips.some((chip) => chip.id === id) ? suggestOrchestrationName(basename, liveTitles) : basename);
  };

  /**
   * #1247 — a draft's Mode chip, chosen as a click chooses it once the Mode row
   * offers it (so an untouched Name follows it, as the TUI's
   * `resuggest_name_for_selection` does), or dropped with a note once the
   * deck's options and the directory's orchestrations have both answered
   * without it. No dependency array: it reads this render's `modes`.
   */
  useEffect(() => {
    const wanted = pendingMode.current;
    if (wanted === undefined || !target) return;
    if (modes.some((candidate) => candidate.id === wanted)) {
      selectMode(wanted as ModeId);
      return;
    }
    const answered = (options !== undefined || optionsError !== undefined) && !orchestrationsPending;
    if (!answered) return;
    pendingMode.current = undefined;
    setRestoreNotes((notes) => [...(notes ?? []), DRAFT_MODE_GONE]);
  });

  /**
   * Voice's "use claude": OVERWRITE Command with that agent's default command
   * and count it as an edit, so a later options reply does not replace it.
   * The field is on screen beside the report, so what was set is visible.
   */
  const commandFromAgent = (agent: NewAgentOption) => {
    setCommand(agent.defaultCommand ?? "");
    commandTouched.current = true;
  };

  /**
   * PRD #1223 M6 — launch the selected orchestration on the chosen deck. The
   * project path and the orchestration name are the deck's own spellings from
   * its answer; the Name is the run's title, sent only when it is not empty,
   * as the TUI sends it. The reply's `agentId` is the START role's.
   */
  const submitOrchestration = async (orchestration: DaemonOrchestration) => {
    if (!deck || orchestrations?.kind !== "project" || titleTaken) return;
    setFormError(undefined);
    setFormCleanup(undefined);
    setPhase("starting");
    const title = orchestrationRunTitle(name, orchestration.name);
    try {
      const result = await runtime.runAction({
        type: "start_orchestration",
        deckId: deck.deckId,
        path: orchestrations.path,
        orchestration: orchestration.name,
        ...(name !== "" ? { displayTitle: name } : {}),
        ...(orchestrations.configRevision ? { configRevision: orchestrations.configRevision } : {}),
      });
      if (result.agentId === undefined) {
        onNotAppeared({ deckName: deck.name, agentName: title });
        return;
      }
      if (!mounted.current) return;
      setPhase("waiting");
      setAwaiting({ deckId: deck.deckId, agentId: result.agentId, deckName: deck.name, agentName: title });
    } catch (cause) {
      failStart(cause);
    }
  };

  const submit = async () => {
    if (!deck || !target || phase !== "idle") return;
    if (selectedOrchestration) {
      await submitOrchestration(selectedOrchestration);
      return;
    }
    setFormError(undefined);
    setFormCleanup(undefined);
    setPhase("starting");
    // Trimmed, as the TUI's `resolve_display_name` trims a plain agent's Name
    // before it becomes `StartAgent.display_name`; a blank one is not sent.
    const agentName = name.trim();
    // An authoring agent's blank Command resolves here, where the TUI resolves
    // it, and the field keeps what the user typed.
    const startCommand = authoringKind ? resolveAuthoringCommand(command, defaultCommand, agents) : command;
    try {
      const result = await runtime.runAction({
        type: "start_agent",
        deckId: deck.deckId,
        cwd: target.path,
        ...(startCommand.trim() ? { command: startCommand } : {}),
        ...(agentName ? { displayName: agentName } : {}),
        ...(authoringKind ? { authoringKind } : {}),
      });
      if (result.agentId === undefined) {
        onNotAppeared({ deckName: deck.name, agentName });
        return;
      }
      if (!mounted.current) return;
      setPhase("waiting");
      setAwaiting({ deckId: deck.deckId, agentId: result.agentId, deckName: deck.name, agentName });
    } catch (cause) {
      failStart(cause);
    }
  };

  /**
   * A spoken "start it" — the Start button, by voice. It fills nothing: it acts
   * on the form as it is, which the user is looking at, and starts it at once.
   *
   * **It used to open a confirmation, and PRD #802 D5's start half was
   * revisited** (PRD #1223, 2026-09-23). The user's words: "it shows another
   * popup asking me to confirm. I think that only introduced friction by me
   * having to give the same instruction twice." What made dropping it safe is
   * recorded in #802's D5 and `docs/develop/voice-first-design.md`: action
   * grounding already requires a start word in the transcript, so a hostile
   * label cannot steer the model into a start; the confirmation re-stated a
   * form that is on screen; and a mistaken agent is one "close" away. The two
   * STOPS keep their confirmations.
   *
   * A start that cannot happen is refused in words: exactly the conditions the
   * Start button is disabled on (no deck, no directory, a start already under
   * way, a run title already live). And an utterance judged against a live
   * form starts only that form — the confirmation's signature check used to
   * guarantee "starts what was shown", and this is its replacement: a deck or
   * directory that moved during the round trip refuses with `FORM_MOVED_ON`,
   * as every fill does. An edit the declaration does not carry, such as a typed
   * Name, is on screen and is started, as the Start button would start it.
   */
  const voiceStart = (dispatch: VoiceDispatchTarget): string | undefined => {
    if (phase !== "idle") return START_IN_FLIGHT;
    if (!deck) return START_NEEDS_DECK;
    if (!target) return START_NEEDS_DIRECTORY;
    const declared = dispatch.declaredForm;
    if (declared && (declared.deckId !== deck.deckId || declared.path !== target.path)) return FORM_MOVED_ON;
    if (titleTaken) return `Nothing was started: ${ORCHESTRATION_TITLE_TAKEN}`;
    void submit();
    return undefined;
  };

  // -- the wait for the fleet (M5) --------------------------------------------
  const settled = useRef(false);
  const callbacks = useRef({ onAppeared, onNotAppeared });
  callbacks.current = { onAppeared, onNotAppeared };
  useEffect(() => {
    if (!awaiting || settled.current) return;
    if (fleetLists(runtime.fleet, awaiting.deckId, awaiting.agentId)) {
      settled.current = true;
      callbacks.current.onAppeared({ deckId: awaiting.deckId, agentId: awaiting.agentId });
    }
  }, [awaiting, runtime.fleet]);
  useEffect(() => {
    if (!awaiting) return;
    const timer = window.setTimeout(() => {
      if (settled.current) return;
      settled.current = true;
      callbacks.current.onNotAppeared({ deckName: awaiting.deckName, agentName: awaiting.agentName });
    }, appearTimeoutMs);
    return () => window.clearTimeout(timer);
  }, [appearTimeoutMs, awaiting]);

  /**
   * `aria-modal="true"` below made true, the way the agent pane makes it true.
   *
   * The attribute says the rest of the interface is unavailable; without this
   * it was not. The overview stays mounted behind the backdrop, so Tab walked
   * straight out of the dialog and into it — the deck groups, the column
   * picker, Refresh, and the New agent button that opened this — and nothing
   * gave focus back to that button on the way out. `useInertBackground` is
   * PRD #1105's answer to exactly that on the agent pane, so this reuses it
   * rather than forking a second, keyboard-only trap beside it: it marks every
   * element that is not an ancestor of this dialog `inert`, which takes the
   * background out of the tab order AND out of hit testing, moves focus inside,
   * and restores it to the opener on close. See that hook for why it walks
   * siblings, and for the one exemption — the voice surface is a peer of a
   * dialog rather than background, and stays reachable here as it does over a
   * pane.
   *
   * The literal `true`: this component is mounted only while the flow is open
   * (`AgentOverview` renders it behind `newAgent &&`), so "open" is its whole
   * lifetime and closing is an unmount. The hook's restore runs from the
   * effect cleanup, which an unmount runs too.
   *
   * Declared BEFORE the focus effect below on purpose. Effects run in
   * declaration order, and the hook captures the opener in the first of its
   * own: called after that one, it would capture whichever control the dialog
   * had just focused and hand focus back to an element of this dialog's that is
   * about to be unmounted. (The open effect above focuses nothing itself: it
   * only files a request that the focus effect carries out.)
   */
  const dialogRef = useInertBackground<HTMLElement>(true);

  // -- focus: the first unsatisfied control, then onward as each is satisfied --
  // On open, the deck field — or, with a deck preselected, the browser; after
  // choosing a deck, the browser; after confirming a directory, the form. Each
  // request is consumed once it lands, so a listing that arrives later (the
  // user browsing) moves nothing.
  useEffect(() => {
    if (!focusRequest) return;
    if (focusRequest.to === "deck") {
      deckListRef.current?.focus();
    } else if (focusRequest.to === "form") {
      nameRef.current?.focus();
    } else {
      if (listingState === "loading" || listingState === "idle") return;
      // With no listing there is nothing to choose, and another deck is the
      // one thing left to do.
      if (listingState === "unsupported" || listingState === "failed") deckListRef.current?.focus();
      // A listing that lands while the user is typing in the filter leaves the
      // caret where it is.
      else if (document.activeElement !== filterRef.current) directoryListRef.current?.focus();
    }
    setFocusRequest(undefined);
  }, [focusRequest, listing, listingState]);
  const activeRowId = `${titleId}-row-${cursor}`;
  useEffect(() => {
    document.getElementById(activeRowId)?.scrollIntoView?.({ block: "nearest" });
  }, [activeRowId, rows]);

  const busy = phase !== "idle";

  // -- keys --------------------------------------------------------------------
  const onDeckKeyDown = (event: KeyboardEvent<HTMLUListElement>) => {
    if (busy) return;
    const eligible = choices.filter((choice) => choice.reason === undefined);
    const index = eligible.findIndex((choice) => choice.deckId === highlight);
    if (event.key === "ArrowDown" || event.key === "j") {
      event.preventDefault();
      if (eligible.length) setHighlight(eligible[(index + 1) % eligible.length].deckId);
    } else if (event.key === "ArrowUp" || event.key === "k") {
      event.preventDefault();
      if (eligible.length) setHighlight(eligible[(index <= 0 ? eligible.length : index) - 1].deckId);
    } else if (event.key === "Enter") {
      event.preventDefault();
      chooseDeck(choices.find((choice) => choice.deckId === highlight));
    }
  };

  const onDirectoryKeyDown = (event: KeyboardEvent<HTMLUListElement>) => {
    if (busy || event.ctrlKey || event.metaKey || event.altKey) return;
    switch (event.key) {
      case "ArrowDown":
      case "j":
        event.preventDefault();
        if (rows.length) setCursor((current) => (current + 1) % rows.length);
        return;
      case "ArrowUp":
      case "k":
        event.preventDefault();
        if (rows.length) setCursor((current) => (current <= 0 ? rows.length : current) - 1);
        return;
      case "Enter":
      case "ArrowRight":
      case "l":
        event.preventDefault();
        openRow();
        return;
      case "ArrowLeft":
      case "Backspace":
      case "h":
        event.preventDefault();
        goUp();
        return;
      case " ":
        event.preventDefault();
        confirmCurrent();
        return;
      case "/":
        event.preventDefault();
        filterRef.current?.focus();
        return;
      case ".":
        // Issue #1240: Show hidden, from the list, where the TUI picker's
        // other keys live — only on a deck that can show them.
        if (!deck?.listingOptions) return;
        event.preventDefault();
        toggleHidden(!showHidden);
        return;
      case "q":
        // Only here, in the browser's list: typed into Name, Command or the
        // filter, `q` is a letter.
        event.preventDefault();
        requestClose();
        return;
      case "Escape":
        // The TUI picker's Esc: a filter is cleared first, and only an
        // unfiltered list cancels.
        if (filter) {
          event.preventDefault();
          event.stopPropagation();
          setFilter("");
        }
        return;
      default:
    }
  };

  const onFilterKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (rows.length) setCursor((current) => (current + 1) % rows.length);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (rows.length) setCursor((current) => (current <= 0 ? rows.length : current) - 1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      directoryListRef.current?.focus();
    } else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      setFilter("");
      directoryListRef.current?.focus();
    } else if (event.key === "Backspace" && filter === "") {
      event.preventDefault();
      directoryListRef.current?.focus();
    }
  };

  /** The TUI form's Left / Right on the Mode row: move to the previous or next chip, wrapping. */
  const onModeKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const index = modes.findIndex((candidate) => candidate.id === mode);
    const next = modes[(index + (event.key === "ArrowRight" ? 1 : modes.length - 1)) % modes.length];
    selectMode(next.id);
    event.currentTarget.querySelector<HTMLButtonElement>(`[data-mode="${next.id}"]`)?.focus();
  };

  const onDialogKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      requestClose();
    }
  };

  const ambiguousChipButtons = ambiguousChips.map((chip) => (
    <button type="button" key={chip.key} className="new-agent-chip is-disabled" disabled title={chip.reason} data-testid={`new-agent-mode-${chip.key}`}>
      {chip.label}
    </button>
  ));

  const highlighted = choices.find((choice) => choice.deckId === highlight);
  const noSubdirectories = listing !== undefined && listing.entries.length === 0;
  /** The form's fields wait for a directory: until one is chosen there is nothing to start in. */
  const formDisabled = busy || !target;

  const deckField = (
    <section className="new-agent-section" aria-labelledby={`${titleId}-deck`} data-testid="new-agent-deck-field">
      <h3 id={`${titleId}-deck`}>Deck</h3>
      {deckNotice && <p className="new-agent-error" role="alert" data-testid="new-agent-deck-notice">{displayText(deckNotice, DISPLAY_LIMITS.message)}</p>}
      <ul
        ref={deckListRef}
        className="new-agent-list is-decks"
        role="listbox"
        aria-labelledby={`${titleId}-deck`}
        aria-disabled={busy || undefined}
        tabIndex={busy ? -1 : 0}
        data-testid="new-agent-deck-list"
        aria-activedescendant={highlighted ? `${titleId}-deck-${choices.indexOf(highlighted)}` : undefined}
        onKeyDown={onDeckKeyDown}
      >
        {choices.map((choice, index) => {
          const disabled = choice.reason !== undefined;
          const chosen = choice.deckId === deck?.deckId;
          return (
            <li
              key={choice.deckId}
              id={`${titleId}-deck-${index}`}
              role="option"
              aria-selected={choice.deckId === highlight}
              aria-disabled={disabled || undefined}
              aria-current={chosen || undefined}
              className={`new-agent-row${choice.deckId === highlight ? " is-active" : ""}${chosen ? " is-chosen" : ""}${disabled ? " is-disabled" : ""}`}
              data-deck-id={choice.deckId}
              data-chosen={chosen || undefined}
              onClick={() => {
                if (!disabled) chooseDeck(choice);
              }}
            >
              {chosen ? <Check size={13} aria-hidden="true" /> : <Server size={13} aria-hidden="true" />}
              <span className="new-agent-row-name">{choice.name}</span>
              <span className="new-agent-row-tag">{choice.deckKind}</span>
              {disabled && <span className="new-agent-row-reason">{choice.reason}</span>}
            </li>
          );
        })}
      </ul>
      {choices.length === 0 && <p className="new-agent-hint">No deck is configured.</p>}
    </section>
  );

  const directoryPanel = (
    <section className="new-agent-section" aria-labelledby={`${titleId}-directory`} data-testid="new-agent-directory-panel">
      <h3 id={`${titleId}-directory`}>Directory</h3>
      {!deck && <p className="new-agent-hint" data-testid="new-agent-directory-idle">Choose a deck to browse its directories.</p>}
      {listingState === "unsupported" && <p className="new-agent-hint" data-testid="new-agent-no-browse">This deck cannot list directories, so no directory can be chosen on it here. Choose another deck.</p>}
      {listingError && <p className="new-agent-error" role="alert" data-testid="new-agent-directory-error">{displayText(listingError, DISPLAY_LIMITS.message)}</p>}
      {listingState === "loading" && <p className="new-agent-hint"><Loader2 className="spin" size={12} /> Listing…</p>}
      {listing && (
        <>
          <div className="new-agent-current">
            <strong data-testid="new-agent-current-path" title={displayText(listing.displayPath, DISPLAY_LIMITS.message)}>{displayText(listing.displayPath, DISPLAY_LIMITS.path)}</strong>
            <input
              ref={filterRef}
              aria-label="Filter"
              data-testid="new-agent-filter"
              value={filter}
              disabled={busy}
              placeholder="/ to filter"
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              onChange={(event) => {
                setFilter(event.target.value);
                setSearchError(undefined);
                setCursor(0);
              }}
              onKeyDown={onFilterKeyDown}
            />
            {deck?.listingOptions && (
              <label className="new-agent-toggle">
                <input type="checkbox" data-testid="new-agent-show-hidden" checked={showHidden} disabled={busy} onChange={(event) => toggleHidden(event.target.checked)} />
                Show hidden
              </label>
            )}
          </div>
          <ul
            ref={directoryListRef}
            className="new-agent-list is-directories"
            role="listbox"
            aria-label="Directories"
            aria-disabled={busy || undefined}
            tabIndex={busy ? -1 : 0}
            data-testid="new-agent-directory-list"
            aria-activedescendant={rows[cursor] ? activeRowId : undefined}
            onKeyDown={onDirectoryKeyDown}
          >
            {rows.map((row, index) => (
              <li
                // Name and path: a symlink shares its target's path (issue #1240).
                key={row.kind === "up" ? ".." : `${row.entry.displayName}\u0000${row.entry.path}`}
                id={`${titleId}-row-${index}`}
                role="option"
                aria-selected={index === cursor}
                className={`new-agent-row${index === cursor ? " is-active" : ""}`}
                data-path={row.kind === "up" ? row.path : row.entry.path}
                onClick={() => {
                  if (busy) return;
                  setCursor(index);
                  if (row.kind === "up") goUp();
                  else if (deck) void loadListing(deck.deckId, row.entry.path);
                }}
              >
                {row.kind === "up"
                  ? <><ArrowUp size={13} aria-hidden="true" /><span className="new-agent-row-name">..</span></>
                  : (
                    <>
                      {row.entry.isProject ? <FolderGit2 size={13} aria-hidden="true" /> : <Folder size={13} aria-hidden="true" />}
                      <span className="new-agent-row-name">{displayText(row.entry.displayName, DISPLAY_LIMITS.name)}</span>
                      {row.entry.isProject && <span className="new-agent-row-tag" data-testid="new-agent-project-mark">project</span>}
                      {row.entry.isSymlink && <span className="new-agent-row-tag" data-testid="new-agent-link-mark" title="A symbolic link: opening it lists the directory it leads to">link</span>}
                    </>
                  )}
              </li>
            ))}
          </ul>
          {noSubdirectories && <p className="new-agent-hint">No subdirectories. Enter or Space uses this directory.</p>}
          {searching && <p className="new-agent-hint" data-testid="new-agent-searching"><Loader2 className="spin" size={12} /> Searching the deck…</p>}
          {searchError && <p className="new-agent-error" role="alert" data-testid="new-agent-search-error">{displayText(searchError, DISPLAY_LIMITS.message)}</p>}
          {searchedHere
            ? searchedHere.truncated && <p className="new-agent-hint" data-testid="new-agent-truncated">{SEARCH_TRUNCATED}</p>
            : listing.truncated && <p className="new-agent-hint" data-testid="new-agent-truncated">{deck?.listingOptions ? TRUNCATED_SEARCHABLE : TRUNCATED_NO_SEARCH}</p>}
          <div className="new-agent-current">
            <p className="new-agent-keys">j/k move · l or Enter opens · h or Backspace goes up · Space uses this directory · / filters{deck?.listingOptions ? " · . shows hidden" : ""} · q closes</p>
            <button type="button" className="button secondary" data-testid="new-agent-use-directory" disabled={busy} onClick={confirmCurrent}><Check size={14} /> Use this directory</button>
          </div>
        </>
      )}
    </section>
  );

  const form = (
    <form
      id={`${titleId}-form`}
      className="new-agent-form new-agent-section"
      data-testid="new-agent-form"
      aria-disabled={!target || undefined}
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="new-agent-field">
        <span>Dir</span>
        <strong data-testid="new-agent-dir" title={target ? displayText(target.displayPath, DISPLAY_LIMITS.message) : undefined}>{target ? displayText(target.displayPath, DISPLAY_LIMITS.path) : <span className="new-agent-unset">No directory chosen yet</span>}</strong>
      </div>
      <div className="new-agent-field">
        <span id={`${titleId}-mode`}>Mode</span>
        <div className="new-agent-chips" role="group" aria-labelledby={`${titleId}-mode`} data-testid="new-agent-modes" onKeyDown={formDisabled ? undefined : onModeKeyDown}>
          {modes.map((candidate, index) => (
            <Fragment key={candidate.id}>
              {index === orchestrationEnd && ambiguousChipButtons}
              <button
                type="button"
                className={`new-agent-chip${candidate.id === mode ? " is-active" : ""}`}
                aria-pressed={candidate.id === mode}
                data-mode={candidate.id}
                data-testid={`new-agent-mode-${candidate.id}`}
                disabled={formDisabled}
                onClick={() => selectMode(candidate.id)}
              >
                {candidate.label}
              </button>
            </Fragment>
          ))}
          {orchestrationEnd >= modes.length && ambiguousChipButtons}
        </div>
      </div>
      {ambiguousReasons.map((reason) => <p key={reason} className="new-agent-hint" data-testid="new-agent-orchestration-ambiguous">{reason}</p>)}
      {orchestrationOffer.withheld && <p className="new-agent-hint" data-testid="new-agent-orchestrations-withheld">{displayText(orchestrationOffer.withheld, DISPLAY_LIMITS.message)}</p>}
      {orchestrationsError && <p className="new-agent-error" data-testid="new-agent-orchestrations-error">{displayText(orchestrationsError, DISPLAY_LIMITS.message)}</p>}
      {authoring.withheld && <p className="new-agent-hint" data-testid="new-agent-authoring-withheld">{authoring.withheld}</p>}
      {optionsError && <p className="new-agent-error" data-testid="new-agent-options-error">{displayText(optionsError, DISPLAY_LIMITS.message)}</p>}
      <label className="new-agent-field">
        <span>Name</span>
        <input
          ref={nameRef}
          data-testid="new-agent-name"
          value={name}
          disabled={formDisabled}
          spellCheck={false}
          onChange={(event) => {
            nameTouched.current = true;
            setName(event.target.value);
          }}
          onKeyDown={(event) => {
            // The TUI form's Enter on Name: move to Command, which submits —
            // or, with Command hidden for an orchestration, submit.
            if (event.key === "Enter" && !selectedOrchestration) {
              event.preventDefault();
              commandRef.current?.focus();
            }
          }}
        />
      </label>
      {titleTaken && <p className="new-agent-error" role="alert" data-testid="new-agent-title-taken">{ORCHESTRATION_TITLE_TAKEN}</p>}
      {!titleTaken && sameDirectory && <p className="new-agent-hint" data-testid="new-agent-same-directory">{SAME_DIRECTORY_ORCHESTRATION}</p>}
      {!selectedOrchestration && <label className="new-agent-field">
        <span>Command</span>
        <input
          ref={commandRef}
          data-testid="new-agent-command"
          value={command}
          disabled={formDisabled}
          placeholder={authoringKind ? `Empty starts ${resolveAuthoringCommand("", defaultCommand, agents)}` : "Empty starts the deck's default shell"}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          onChange={(event) => {
            commandTouched.current = true;
            setCommand(event.target.value);
          }}
        />
      </label>}
      {formCleanup && formCleanup.length > 0 && <CleanupWarning stops={formCleanup} testId="new-agent-cleanup-warning" className="new-agent-error new-agent-cleanup" />}
      {formError && <p className="new-agent-error" role="alert" data-testid="new-agent-error">{displayText(formError, DISPLAY_LIMITS.message)}</p>}
      {formError && displayText(formError, DISPLAY_LIMITS.detail) !== displayText(formError, DISPLAY_LIMITS.message) && (
        <details className="new-agent-detail" data-testid="new-agent-error-detail">
          {/* "Detail", not "Full detail" (PRD #1223 audit V7): this copy is
              itself clamped at `DISPLAY_LIMITS.detail`, so a longer sentence
              is not shown in full here either. */}
          <summary>Detail</summary>
          <p>{displayText(formError, DISPLAY_LIMITS.detail)}</p>
        </details>
      )}
      {/*
          `role="status"`, because in this one phase there is nothing left to
          read it off. Audit F5 blocks every close route while a start is in
          flight by DISABLING the controls — the header's close button, the
          deck field, the directory panel, Discard, Start and the fields all at once —
          and a disabled button is neither focusable nor announced, so the
          close button's `title` explaining the block is invisible to a screen
          reader. Focus falls back to the dialog itself (see `tabIndex` below,
          and `useInertBackground`, which moves it there when the pressed Start
          is blurred), and this is what tells a listener why nothing answers.
          There is no Cancel button (PRD #1223 U2): it was the same close as the
          header's X, and no other dialog here has both. Discard (#1247) is not
          one: since every close keeps the form as a draft, it is the one
          control that throws the form away, which the X does not.
      */}
      {starting && <p className="new-agent-hint" role="status" data-testid="new-agent-starting"><Loader2 className="spin" size={12} /> {STARTING_CLOSE_BLOCKED}</p>}
      {phase === "waiting" && <p className="new-agent-hint" data-testid="new-agent-waiting"><Loader2 className="spin" size={12} /> Started. Waiting for the deck to list it…</p>}
    </form>
  );

  return (
    <div className="dialog-backdrop" role="presentation" data-testid="new-agent-backdrop" onMouseDown={requestClose}>
      <section
        ref={dialogRef}
        className="new-agent-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="new-agent-dialog"
        /* A focus TARGET, never a tab stop, as on the agent pane: the hook puts
           focus here when it opens over a control that had it, and Tab then
           proceeds into the dialog's own controls. It is also where focus lands
           while a start is in flight, when every control inside is disabled and
           there is nothing else for it to hold. */
        tabIndex={-1}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={onDialogKeyDown}
      >
        <header>
          <h2 id={titleId}>New agent</h2>
          {deck && <span className="new-agent-deck" data-testid="new-agent-chosen-deck">{deck.name}</span>}
          <button type="button" className="icon-button" aria-label="Close new agent" disabled={starting} title={starting ? STARTING_CLOSE_BLOCKED : undefined} onClick={requestClose}><X size={15} /></button>
        </header>
        {/* Top to bottom in the tab order the wizard's steps had: deck, the
            directory browser, then Mode, Name, Command and Start. */}
        <div className="new-agent-body">
          {restoreNotes && (
            <div className="new-agent-restored" role="status" data-testid="new-agent-restored">
              {restoreNotes.map((note) => <p key={note} className="new-agent-hint">{note}</p>)}
            </div>
          )}
          {deckField}
          {directoryPanel}
          {form}
        </div>
        <footer>
          {/* #1247 — the one control that throws the form away; every close
              keeps it. Named for voice's `discard_new_agent`, whose words are
              this label and its accessible name. */}
          <button type="button" className="button secondary" data-testid="new-agent-discard" aria-label="Discard new agent" title={starting ? STARTING_CLOSE_BLOCKED : "Close and forget what was entered in this form"} disabled={starting} onClick={requestDiscard}>
            <Trash2 size={14} /> Discard
          </button>
          <button type="submit" form={`${titleId}-form`} className="button primary" data-testid="new-agent-start" disabled={formDisabled || titleTaken}>
            {phase === "idle" ? <><Plus size={14} /> {selectedOrchestration ? "Start orchestration" : "Start agent"}</> : phase === "starting" ? "Starting…" : "Opening…"}
          </button>
        </footer>
      </section>
    </div>
  );
}
