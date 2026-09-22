import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { ArrowUp, Check, Folder, FolderGit2, Loader2, Plus, Server, X } from "lucide-react";
import { LaunchCleanupError } from "../lib/actionError";
import { CleanupWarning } from "./CleanupWarning";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { useInertBackground } from "../hooks/useInertBackground";
import {
  ambiguousOrchestrationReason,
  AUTHORING_MODES,
  authoringModes,
  deckChoices,
  directoryLabel,
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
  onClose: () => void;
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

/**
 * The Mode row's first chip — a plain agent. Then, in the TUI cycler's order,
 * one chip per orchestration the directory's project defines on the deck (PRD
 * #1223 M6, from {@link orchestrationModes}), then the authoring kinds the deck
 * can start (PRD #1223 M7, from {@link authoringModes}).
 */
const NO_MODE = { id: "none", label: "No mode" } as const;

type ModeId = typeof NO_MODE.id | AuthoringKind | ReturnType<typeof orchestrationModeId>;

const AUTO_AGENT = "auto";

/** Why the dialog cannot be closed during a start (PRD #1223 audit F5). */
export const STARTING_CLOSE_BLOCKED = "Waiting for the deck to answer the start. The dialog can be closed once it has.";

/*
  PRD #1223 — the directory browser's refusals by voice, in the dialog's own
  words. Rust already refuses a directory row when no browser was declared, so
  each of these answers a browser that changed during the round trip.
*/
/** No browser to move: the dialog is closed, has no listing, or is starting. */
export const NO_DIRECTORY_BROWSER = "The New agent dialog is not showing a directory listing, so nothing was changed.";
/** The browser moved between the utterance and its answer. */
export const DIRECTORY_MOVED_ON = "The directory browser moved on while that was being worked out, so nothing was changed. Say it again.";
/** The named child is no longer in the listing on screen. */
export const DIRECTORY_NOT_LISTED = "That directory is not in the listing any more, so nothing was opened.";
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
/** The entry is not in the Agent picker any more. */
export const AGENT_TYPE_NOT_OFFERED = "That agent is not in this form's picker any more, so the agent was not changed.";
/** The start confirmation is showing; a fill would change what it describes. */
export const FORM_UNDER_CONFIRMATION = "The start confirmation is open, so the form was not changed. Answer it first.";
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
/** The start confirmation is already showing. */
export const START_AWAITING_CONFIRMATION = "The start is already waiting for your confirmation in the dialog. Nothing else was started.";
/** The form changed between the confirmation opening and being confirmed. */
export const START_FORM_CHANGED = "The form changed after the confirmation opened, so nothing was started. Check it and start again.";

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
 * 3. **Form** — Mode, Agent, Name and Command, prefilled in the TUI's order and
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
export function NewAgentDialog({ runtime, initialDeckId, onClose, onAppeared, onNotAppeared, appearTimeoutMs = NEW_AGENT_APPEAR_TIMEOUT_MS, closeRequest, voice }: NewAgentDialogProps) {
  const titleId = useId();
  const choices = useMemo(() => deckChoices(runtime.fleet), [runtime.fleet]);
  /** The deck field's cursor — moved by `j`/`k`, and not a choice until Enter or a click. */
  const [highlight, setHighlight] = useState<string | undefined>(() => preselectedDeck(deckChoices(runtime.fleet), initialDeckId));
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

  // -- form -----------------------------------------------------------------------
  const [target, setTarget] = useState<{ path: string; displayPath: string }>();
  const [options, setOptions] = useState<NewAgentOptions>();
  const [optionsError, setOptionsError] = useState<string>();
  const [agentChoice, setAgentChoice] = useState<string>(AUTO_AGENT);
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
  /** A human edited Name — a generated default may replace a generated default, never an edit. */
  const nameTouched = useRef(false);
  /**
   * Whether each path the deck listed holds a project, from the listings seen
   * so far — the marker that decides whether the deck is asked for
   * orchestrations at all. A directory reached another way (by going up) has
   * no entry here and is asked, and the deck's `not_project` answer covers it.
   */
  const projectMarks = useRef(new Map<string, boolean>());
  const [phase, setPhase] = useState<"idle" | "starting" | "waiting">("idle");
  /**
   * PRD #802 D5 — the confirmation a SPOKEN start opens, and the form it was
   * opened on. The manual Start button never opens it: it starts as it always
   * has. See `voiceRequestStart`.
   */
  const [startConfirm, setStartConfirm] = useState<{ state: ConfirmState; signature: string }>();
  const [awaiting, setAwaiting] = useState<{ deckId: string; agentId: string; deckName: string; agentName: string }>();

  const deckListRef = useRef<HTMLUListElement>(null);
  const directoryListRef = useRef<HTMLUListElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const commandRef = useRef<HTMLInputElement>(null);

  /**
   * Everything that hangs off the chosen deck, dropped: the directory panel,
   * the chosen directory, the deck's options and orchestrations, and every
   * reply still in flight for them. `keepEdits` keeps a Name or Command the
   * user typed — a voluntary change of deck — and otherwise the form is
   * cleared as well.
   */
  const clearDeckDependents = useCallback((keepEdits: boolean) => {
    listingSeq.current += 1;
    optionsSeq.current += 1;
    orchestrationsSeq.current += 1;
    setListing(undefined);
    setListingState("idle");
    setListingError(undefined);
    setFilter("");
    setCursor(0);
    setTarget(undefined);
    setOptions(undefined);
    setOptionsError(undefined);
    setOrchestrations(undefined);
    setOrchestrationsError(undefined);
    setAgentChoice(AUTO_AGENT);
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
   * `close` (PRD #1223 U5) — and none of them works while
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
  /** Close, or answer why not. Every route calls this; only voice reads the answer. */
  const requestClose = (): string | undefined => {
    if (starting) return STARTING_CLOSE_BLOCKED;
    onClose();
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

  const rows = useMemo<DirectoryRow[]>(() => {
    if (!listing) return [];
    const up: DirectoryRow[] = listing.parent === undefined ? [] : [{ kind: "up", path: listing.parent }];
    return [...up, ...filterDirectoryEntries(listing.entries, filter).map((entry): DirectoryRow => ({ kind: "entry", entry }))];
  }, [filter, listing]);

  /**
   * List `path` on the captured deck (its home when absent), then put the
   * cursor on `focusPath` — the directory just left, when going up — or else
   * on the first subdirectory rather than on `..`.
   *
   * `homeOnFailure` is the deck's configured default directory's fallback
   * (PRD #1223): the deck vetted that path when it answered the options query,
   * but it can vanish before the listing lands, and a browser that opened on an
   * error would be a worse start than the home it replaces.
   */
  const loadListing = useCallback(async (deckId: string, path?: string, focusPath?: string, homeOnFailure = false) => {
    const seq = ++listingSeq.current;
    setListingError(undefined);
    setListingState((current) => (current === "ready" ? current : "loading"));
    try {
      const reply = await runtime.listDirectories(deckId, path);
      if (seq !== listingSeq.current) return;
      if (reply.kind === "unsupported") {
        setListing(undefined);
        setListingState("unsupported");
        return;
      }
      for (const entry of reply.entries) projectMarks.current.set(entry.path, entry.isProject);
      setListing(reply);
      setListingState("ready");
      setFilter("");
      const offset = reply.parent === undefined ? 0 : 1;
      const focused = focusPath === undefined ? -1 : reply.entries.findIndex((entry) => entry.path === focusPath);
      setCursor(focused >= 0 ? focused + offset : reply.entries.length > 0 ? offset : 0);
    } catch (cause) {
      if (seq !== listingSeq.current) return;
      const message = messageOf(cause);
      if (isDeckGoneError(message)) {
        deckGone(message);
        return;
      }
      if (homeOnFailure && path !== undefined) {
        void loadListing(deckId);
        return;
      }
      setListingError(message);
      setListingState((current) => (current === "ready" ? current : "failed"));
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
   */
  const chooseDeck = (choice: DeckChoice | undefined) => {
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
      void loadListing(deckId, startAt, undefined, startAt !== undefined);
    })();
  };

  /**
   * The deck field's preselection — the only eligible deck, or the one whose
   * header opened the flow — is CHOSEN on open, so its listing loads at once
   * and there is no Next to press. Otherwise focus starts on the deck field,
   * the first control left unsatisfied.
   */
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current) return;
    opened.current = true;
    const preselected = choices.find((choice) => choice.deckId === highlight);
    if (preselected && preselected.reason === undefined) chooseDeck(preselected);
    else setFocusRequest({ to: "deck" });
  });

  /**
   * A directory the deck returned, chosen: the Name follows it while untouched,
   * the Mode goes back to No mode as every fresh TUI form does, and the deck is
   * asked for its orchestrations. Agent and Command stay — they hang off the
   * deck, not the directory.
   */
  const confirmDirectory = (path: string, displayPath: string) => {
    if (!deck || phase !== "idle") return;
    const deckId = deck.deckId;
    setTarget({ path, displayPath });
    if (!nameTouched.current) setName(directoryLabel(path));
    setModeChoice(NO_MODE.id);
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
    if (queryOrchestrations && projectMarks.current.get(path) !== false) {
      void (async () => {
        try {
          const answer = await queryOrchestrations(deckId, path);
          if (orchestrationsSeqNow !== orchestrationsSeq.current) return;
          setOrchestrations(answer);
        } catch (cause) {
          if (orchestrationsSeqNow !== orchestrationsSeq.current) return;
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
   * control inside the dialog is disabled then.
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
    const entry = listing.entries.find((candidate) => candidate.path === target.directoryPath);
    if (!entry) return DIRECTORY_NOT_LISTED;
    // A click on the row: the cursor lands on it (when the filter shows it),
    // then the deck lists it.
    const index = rows.findIndex((row) => row.kind === "entry" && row.entry.path === entry.path);
    if (index >= 0) setCursor(index);
    void loadListing(deck.deckId, entry.path);
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
   * PRD #1223 — the rest of the form by voice: Mode, Agent and Name. Each calls
   * what the control's own click or keystroke calls (`selectMode`,
   * `chooseAgent`, the Name input's setter) and nothing else, so a spoken
   * choice of agent overwrites Command exactly as the picker does, and a spoken
   * Name is an edit exactly as a typed one is.
   *
   * Each first re-checks that the form is still the one the utterance was
   * judged against — its deck and its chosen directory — for the browser's
   * reason, and refuses in the dialog's words when it is not.
   *
   * Command has no move: it is the field that executes, and it stays typed.
   */
  const formMovedOn = (dispatch: VoiceDispatchTarget): string | undefined => {
    if (!deck || !target || phase !== "idle") return NO_NEW_AGENT_FORM;
    if (startConfirm) return FORM_UNDER_CONFIRMATION;
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
    const id = dispatch.agentTypeId;
    if (id !== AUTO_AGENT && !agents.some((candidate) => candidate.id === id)) return AGENT_TYPE_NOT_OFFERED;
    chooseAgent(id ?? AUTO_AGENT);
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
   * Whether the form's fields are live — what a click on one of them needs —
   * and, for voice, not under a start confirmation: a fill while the
   * confirmation is showing would change what it describes behind its back.
   */
  const formLive = deck !== undefined && target !== undefined && phase === "idle" && startConfirm === undefined;

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
            agentTypes: [{ id: AUTO_AGENT, label: AUTO_AGENT }, ...agents.map((agent) => ({ id: agent.id, label: agent.displayName }))],
            /* The authoring chips this form withholds — for a deck that cannot
               compose them, or `schedule: issues` with its flag off — so a
               spoken one is refused BY NAME instead of becoming the nearest
               offered chip. Resolvable never; see `withheld_modes` in Rust. */
            withheldModes: AUTHORING_MODES.filter((candidate) => !authoring.offered.some((offered) => offered.kind === candidate.kind)).map(({ kind, label }) => ({ id: kind, label })),
          }
          : undefined,
      },
      chooseNewAgentMode: voiceChooseMode,
      chooseNewAgentType: voiceChooseAgentType,
      nameNewAgent: voiceNameNewAgent,
      confirmStartNewAgent: voiceRequestStart,
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
    setModeChoice(id);
    if (nameTouched.current || !target) return;
    const basename = directoryLabel(target.path);
    setName(orchestrationChips.some((chip) => chip.id === id) ? suggestOrchestrationName(basename, liveTitles) : basename);
  };

  const chooseAgent = (id: string) => {
    setAgentChoice(id);
    const agent = agents.find((candidate) => candidate.id === id);
    // The TUI's `select_agent`: choosing an agent OVERWRITES Command with its
    // default command. Going back to `auto` leaves Command as it is.
    if (agent) {
      setCommand(agent.defaultCommand ?? "");
      commandTouched.current = true;
    }
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
   * PRD #802 D5 — a spoken "start it": it fills nothing and STARTS NOTHING. It
   * acts on the form as it is and opens a confirmation whose body states the
   * risk — the deck, the directory, the mode and the command that will run —
   * and only that confirmation's button, pressed by hand, calls `submit`.
   *
   * A start that cannot happen is refused in words rather than confirmed:
   * exactly the conditions the Start button is disabled on (no deck, no
   * directory, a start already under way, a run title already live), plus a
   * confirmation already showing.
   *
   * **Confirming starts exactly what the confirmation showed.** The form's
   * signature is captured when it opens and compared when it is confirmed;
   * if anything moved in between — the options landing, a key pressed behind
   * the scrim — the confirmation starts nothing and says so. `submitRef` is
   * the LATEST `submit`, so an unchanged form is started from its live state.
   */
  const formSignature = JSON.stringify([deck?.deckId, target?.path, mode, agentChoice, name, selectedOrchestration ? null : command]);
  const liveSignature = useRef(formSignature);
  liveSignature.current = formSignature;
  const submitRef = useRef(submit);
  submitRef.current = submit;
  const voiceRequestStart = (): string | undefined => {
    if (startConfirm) return START_AWAITING_CONFIRMATION;
    if (phase !== "idle") return START_IN_FLIGHT;
    if (!deck) return START_NEEDS_DECK;
    if (!target) return START_NEEDS_DIRECTORY;
    if (titleTaken) return `Nothing was started: ${ORCHESTRATION_TITLE_TAKEN}`;
    const signature = formSignature;
    const where = displayText(target.displayPath, DISPLAY_LIMITS.path);
    let title: string;
    let body: string;
    if (selectedOrchestration) {
      const orchestrationName = displayText(selectedOrchestration.displayName, DISPLAY_LIMITS.name);
      const roles = selectedOrchestration.roles.map((role) => displayText(role.displayName, DISPLAY_LIMITS.name)).join(", ");
      title = `Start the ${orchestrationName} orchestration?`;
      body = `You asked by voice to start the ${orchestrationName} orchestration on ${deck.name} in ${where}. Every role starts — ${roles} — each running the command its configuration gives it, as the deck's user, under the run title “${displayText(runTitle ?? "", DISPLAY_LIMITS.name)}”. Nothing has started yet.`;
    } else {
      const startCommand = authoringKind ? resolveAuthoringCommand(command, defaultCommand, agents) : command;
      const runs = startCommand.trim() ? `runs ${displayText(startCommand, DISPLAY_LIMITS.message)}` : "runs the deck's default shell";
      const modeLabel = modes.find((candidate) => candidate.id === mode)?.label ?? NO_MODE.label;
      const named = name.trim() ? ` named “${displayText(name.trim(), DISPLAY_LIMITS.name)}”` : "";
      title = "Start this agent?";
      body = `You asked by voice to start an agent${named} on ${deck.name} in ${where}. Mode: ${modeLabel}. It ${runs} as the deck's user, with that directory's access, until it is stopped. Nothing has started yet.`;
    }
    setStartConfirm({
      signature,
      state: {
        title,
        body,
        label: selectedOrchestration ? "Start orchestration" : "Start agent",
        busyLabel: "Starting…",
        action: async () => {
          if (liveSignature.current !== signature) {
            setFormError(START_FORM_CHANGED);
            return;
          }
          await submitRef.current();
        },
      },
    });
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
      // Esc answers the innermost thing: a start confirmation is cancelled,
      // and the dialog under it stays.
      if (startConfirm) setStartConfirm(undefined);
      else requestClose();
    }
  };

  const ambiguousChipButtons = ambiguousChips.map((chip) => (
    <button type="button" key={chip.key} className="new-agent-chip is-disabled" disabled title={chip.reason} data-testid={`new-agent-mode-${chip.key}`}>
      {chip.label}
    </button>
  ));
  const unsupportedOptions = options?.kind === "unsupported";

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
                setCursor(0);
              }}
              onKeyDown={onFilterKeyDown}
            />
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
                key={row.kind === "up" ? ".." : row.entry.path}
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
                    </>
                  )}
              </li>
            ))}
          </ul>
          {noSubdirectories && <p className="new-agent-hint">No subdirectories. Enter or Space uses this directory.</p>}
          {listing.truncated && <p className="new-agent-hint" data-testid="new-agent-truncated">Not every subdirectory is listed: the deck stopped at its limit, and the ones past it cannot be chosen here.</p>}
          <div className="new-agent-current">
            <p className="new-agent-keys">j/k move · l or Enter opens · h or Backspace goes up · Space uses this directory · / filters · q closes</p>
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
      <label className="new-agent-field">
        <span>Agent</span>
        <select data-testid="new-agent-agent" value={agentChoice} disabled={formDisabled} onChange={(event) => chooseAgent(event.target.value)}>
          <option value={AUTO_AGENT}>auto</option>
          {agents.map((agent) => <option key={agent.id} value={agent.id}>{displayText(agent.displayName, DISPLAY_LIMITS.name)}</option>)}
        </select>
      </label>
      {unsupportedOptions && <p className="new-agent-hint" data-testid="new-agent-desktop-registry">This deck does not report its agents. The list is this app's own.</p>}
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
          deck field, the directory panel, Start and the fields all at once —
          and a disabled button is neither focusable nor announced, so the
          close button's `title` explaining the block is invisible to a screen
          reader. Focus falls back to the dialog itself (see `tabIndex` below,
          and `useInertBackground`, which moves it there when the pressed Start
          is blurred), and this is what tells a listener why nothing answers.
          There is no Cancel button (PRD #1223 U2): it was the same close as the
          header's X, and no other dialog here has both.
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
            directory browser, then Mode, Agent, Name, Command and Start. */}
        <div className="new-agent-body">
          {deckField}
          {directoryPanel}
          {form}
        </div>
        <footer>
          <button type="submit" form={`${titleId}-form`} className="button primary" data-testid="new-agent-start" disabled={formDisabled || titleTaken}>
            {phase === "idle" ? <><Plus size={14} /> {selectedOrchestration ? "Start orchestration" : "Start agent"}</> : phase === "starting" ? "Starting…" : "Opening…"}
          </button>
        </footer>
        {/* PRD #802 D5 — a spoken start's confirmation. INSIDE the dialog, so
            `useInertBackground` leaves it reachable; its scrim covers the form,
            and its Cancel and Esc return to it unchanged. */}
        {startConfirm && <ConfirmDialog state={startConfirm.state} onClose={() => setStartConfirm(undefined)} />}
      </section>
    </div>
  );
}
