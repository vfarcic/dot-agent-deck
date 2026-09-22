import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { ArrowLeft, ArrowUp, Check, Folder, FolderGit2, Loader2, Plus, X } from "lucide-react";
import { LaunchCleanupError } from "../lib/actionError";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  ambiguousOrchestrationReason,
  AUTHORING_MODES,
  authoringModes,
  cleanupWarning,
  deckChoices,
  directoryLabel,
  filterDirectoryEntries,
  fleetLists,
  isAbsoluteTypedPath,
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
  TYPED_PATH_SHAPE_REFUSAL,
  type DeckChoice,
} from "../lib/newAgent";
import type { AuthoringKind, DaemonOrchestration, DeckDirectoryEntry, DeckDirectoryListing, DeckRuntimeState, NewAgentOption, NewAgentOptions, NewAgentOrchestrations } from "../types";

/**
 * What the dialog needs from the runtime. The two queries are REQUIRED here
 * although they are optional on `DeckRuntimeState`: the overview renders no
 * entry point into this dialog for a runtime without them.
 */
export type NewAgentRuntime = Pick<DeckRuntimeState, "fleet" | "runAction" | "clearError"> & Required<Pick<DeckRuntimeState, "listDirectories" | "newAgentOptions">> & Pick<DeckRuntimeState, "newAgentOrchestrations">;

export interface NewAgentDialogProps {
  runtime: NewAgentRuntime;
  /** The deck the flow was opened from — a deck header's affordance — which the deck step preselects when it can take a spawn. */
  initialDeckId?: string;
  onClose: () => void;
  /** The started agent is listed by its deck: the caller opens its pane. */
  onAppeared: (target: { deckId: string; agentId: string }) => void;
  /** The deck accepted the start and has not listed the agent within the bound. */
  onNotAppeared: (report: { deckName: string; agentName: string }) => void;
  /** Test seam; production uses {@link NEW_AGENT_APPEAR_TIMEOUT_MS}. */
  appearTimeoutMs?: number;
}

type Listing = Extract<DeckDirectoryListing, { kind: "listing" }>;

/** One row of the directory step: the parent (`..`), or a subdirectory. */
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
const STARTING_CLOSE_BLOCKED = "Waiting for the deck to answer the start. The dialog can be closed once it has.";

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * PRD #1223 M4/M5 — the desktop's New agent flow: the TUI's `Ctrl+n` with a
 * deck step in front of it, because the desktop drives several decks.
 *
 * 1. **Deck** — every deck in the fleet; the ones that cannot take a spawn are
 *    shown disabled with the reason the overview gives for them.
 * 2. **Directory** — that deck's filesystem, browsed one level per request with
 *    the TUI picker's keys, plus a typed path — the only mode on a deck without
 *    the listing verb.
 * 3. **Form** — Mode, Agent, Name and Command, prefilled in the TUI's order.
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
 * Confirming the deck step captures its wire `deckId`, and every later request
 * — listing, options, start — carries that value. Nothing reads the selected
 * deck. A request refused because the deck has left the fleet returns the flow
 * to the deck step with the refusal, rather than aiming the next request
 * anywhere else.
 *
 * # No path is built here
 *
 * Every path the flow sends is one the deck returned — a listing's `path`,
 * `parent`, or an entry's `path` — or one the user typed, sent verbatim. The
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
export function NewAgentDialog({ runtime, initialDeckId, onClose, onAppeared, onNotAppeared, appearTimeoutMs = NEW_AGENT_APPEAR_TIMEOUT_MS }: NewAgentDialogProps) {
  const titleId = useId();
  const choices = useMemo(() => deckChoices(runtime.fleet), [runtime.fleet]);
  const [step, setStep] = useState<"deck" | "directory" | "form">("deck");
  const [highlight, setHighlight] = useState<string | undefined>(() => preselectedDeck(deckChoices(runtime.fleet), initialDeckId));
  const [deckNotice, setDeckNotice] = useState<string>();
  /** The deck the flow is about — captured once, at the deck step. */
  const [deck, setDeck] = useState<DeckChoice>();

  // -- directory step ---------------------------------------------------------
  const [listing, setListing] = useState<Listing>();
  const [listingState, setListingState] = useState<"loading" | "ready" | "unsupported" | "failed">("loading");
  const [listingError, setListingError] = useState<string>();
  const [cursor, setCursor] = useState(0);
  const [filter, setFilter] = useState("");
  const [typedPath, setTypedPath] = useState("");
  /** Drops the reply of any listing request a later one has superseded. */
  const listingSeq = useRef(0);
  /** A typed path was just listed: hand the keyboard back to the listing, so Space uses it. */
  const focusListingNext = useRef(false);

  // -- form step --------------------------------------------------------------
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
   * orchestrations at all. A directory reached another way (typed, or by going
   * up) has no entry here and is asked, and the deck's `not_project` answer
   * covers it.
   */
  const projectMarks = useRef(new Map<string, boolean>());
  const [phase, setPhase] = useState<"idle" | "starting" | "waiting">("idle");
  const [awaiting, setAwaiting] = useState<{ deckId: string; agentId: string; deckName: string; agentName: string }>();

  const deckListRef = useRef<HTMLUListElement>(null);
  const directoryListRef = useRef<HTMLUListElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const pathRef = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const commandRef = useRef<HTMLInputElement>(null);

  /**
   * Back to the deck step, because the chosen deck is no longer one this app
   * observes. The message is the refusal itself; the highlight is recomputed
   * over the fleet as it is now, never carried over to another deck.
   */
  const returnToDeckStep = useCallback((message: string) => {
    listingSeq.current += 1;
    setDeck(undefined);
    setStep("deck");
    setDeckNotice(message);
    setPhase("idle");
    setAwaiting(undefined);
    setHighlight(preselectedDeck(deckChoices(runtime.fleet)));
  }, [runtime.fleet]);

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
   * Every way out of the dialog — Cancel, the header's close button, Esc, a
   * backdrop click and the directory step's `q` — and none of them works while
   * a start is in flight (PRD #1223 audit F5). Closing then would unmount the
   * one place a failure is explained, while the action itself carries on; the
   * wait is bounded instead, because every deck call a start makes is (audit
   * F4). Once the deck has answered — the "waiting for the fleet" phase
   * included — closing works as before.
   */
  const starting = phase === "starting";
  const requestClose = () => {
    if (!starting) onClose();
  };

  /**
   * A start the deck refused. The runtime files a failed action under its
   * global error as well; a MOUNTED dialog says it here, beside the values, so
   * that copy is dropped. An unmounted one leaves it alone — see `mounted`.
   */
  const failStart = (cause: unknown) => {
    if (!mounted.current) return;
    runtime.clearError();
    const message = messageOf(cause);
    // PRD #1223 audit V3: the structured cleanup is read FIRST. The deck step
    // shows the refusal as prose and nothing else, so classifying such a
    // failure as deck loss would drop the roles that may still be running —
    // and `isDeckGoneError` reads the start of the message for the same
    // reason, since a role name is interpolated into this sentence.
    const cleanup = cause instanceof LaunchCleanupError ? cause.unconfirmedStops : undefined;
    if (cleanup === undefined && isDeckGoneError(message)) {
      returnToDeckStep(message);
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
   */
  const loadListing = useCallback(async (deckId: string, path?: string, focusPath?: string) => {
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
      setTypedPath("");
      const offset = reply.parent === undefined ? 0 : 1;
      const focused = focusPath === undefined ? -1 : reply.entries.findIndex((entry) => entry.path === focusPath);
      setCursor(focused >= 0 ? focused + offset : reply.entries.length > 0 ? offset : 0);
    } catch (cause) {
      if (seq !== listingSeq.current) return;
      const message = messageOf(cause);
      focusListingNext.current = false;
      if (isDeckGoneError(message)) {
        returnToDeckStep(message);
        return;
      }
      setListingError(message);
      setListingState((current) => (current === "ready" ? current : "failed"));
    }
  }, [returnToDeckStep, runtime]);

  const confirmDeck = (choice: DeckChoice | undefined) => {
    if (!choice || choice.reason !== undefined) return;
    setDeck(choice);
    setDeckNotice(undefined);
    setListing(undefined);
    setListingState("loading");
    setStep("directory");
    void loadListing(choice.deckId);
  };

  /** Into the form, for a directory the deck returned — or, on a deck without the listing verb, the path the user typed. */
  const confirmDirectory = (path: string, displayPath: string) => {
    if (!deck) return;
    const deckId = deck.deckId;
    setTarget({ path, displayPath });
    setName(directoryLabel(path));
    nameTouched.current = false;
    setCommand("");
    commandTouched.current = false;
    setAgentChoice(AUTO_AGENT);
    setModeChoice(NO_MODE.id);
    setOptions(undefined);
    setOptionsError(undefined);
    setOrchestrations(undefined);
    setOrchestrationsError(undefined);
    setFormError(undefined);
    setFormCleanup(undefined);
    setStep("form");
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
          if (isDeckGoneError(message)) returnToDeckStep(message);
          else setOrchestrationsError(message);
        }
      })();
    }
    const seq = ++optionsSeq.current;
    void (async () => {
      try {
        const answer = await runtime.newAgentOptions(deckId);
        if (seq !== optionsSeq.current) return;
        setOptions(answer);
        if (!commandTouched.current) {
          setCommand(answer.kind === "deck" ? seedCommand(answer.defaultCommand, answer.lastCommand) : seedCommand(undefined, answer.lastCommand));
        }
      } catch (cause) {
        if (seq !== optionsSeq.current) return;
        const message = messageOf(cause);
        if (isDeckGoneError(message)) returnToDeckStep(message);
        else setOptionsError(message);
      }
    })();
  };

  const confirmCurrent = () => {
    if (listing) confirmDirectory(listing.path, listing.displayPath);
  };

  const goUp = () => {
    if (deck && listing?.parent !== undefined) void loadListing(deck.deckId, listing.parent, listing.path);
  };

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

  const submitTypedPath = () => {
    if (!deck || typedPath === "") return;
    // PRD #1223 audit D2: a relative path is refused here, in the crate's own
    // sentence, before anything is asked. It matters most on a deck without the
    // listing verb, where the typed path is what the start sends — and the
    // crate refuses it there too, so this only saves the round trip.
    if (!isAbsoluteTypedPath(typedPath)) {
      setListingError(TYPED_PATH_SHAPE_REFUSAL);
      return;
    }
    // Verbatim: the deck canonicalises it, and its reply is what the flow
    // carries. A deck without the listing verb has no reply to give, so the
    // typed path itself goes to the form and the start is where the deck
    // accepts or refuses it.
    if (listingState === "unsupported") {
      setListingError(undefined);
      confirmDirectory(typedPath, displayText(typedPath, DISPLAY_LIMITS.path));
      return;
    }
    focusListingNext.current = true;
    void loadListing(deck.deckId, typedPath);
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

  // -- focus: each step opens with its main control focused -------------------
  useEffect(() => {
    if (step === "deck") deckListRef.current?.focus();
    else if (step === "form") nameRef.current?.focus();
  }, [step]);
  useEffect(() => {
    if (step !== "directory") return;
    if (listingState === "unsupported" || listingState === "failed") {
      pathRef.current?.focus();
      return;
    }
    if (listingState !== "ready") return;
    // A listing that lands while the user is typing in the filter or the path
    // field leaves the caret where it is — unless it is the typed path's own.
    const typing = document.activeElement === filterRef.current || document.activeElement === pathRef.current;
    if (focusListingNext.current || !typing) directoryListRef.current?.focus();
    focusListingNext.current = false;
  }, [listing, listingState, step]);
  const activeRowId = `${titleId}-row-${cursor}`;
  useEffect(() => {
    document.getElementById(activeRowId)?.scrollIntoView?.({ block: "nearest" });
  }, [activeRowId, rows]);

  // -- keys --------------------------------------------------------------------
  const onDeckKeyDown = (event: KeyboardEvent<HTMLUListElement>) => {
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
      confirmDeck(choices.find((choice) => choice.deckId === highlight));
    }
  };

  const onDirectoryKeyDown = (event: KeyboardEvent<HTMLUListElement>) => {
    if (event.ctrlKey || event.metaKey || event.altKey) return;
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

  const busy = phase !== "idle";
  const ambiguousChipButtons = ambiguousChips.map((chip) => (
    <button type="button" key={chip.key} className="new-agent-chip is-disabled" disabled title={chip.reason} data-testid={`new-agent-mode-${chip.key}`}>
      {chip.label}
    </button>
  ));
  const unsupportedOptions = options?.kind === "unsupported";

  let body: ReactNode;
  let footer: ReactNode;
  if (step === "deck") {
    const highlighted = choices.find((choice) => choice.deckId === highlight);
    body = (
      <>
        {deckNotice && <p className="new-agent-error" role="alert" data-testid="new-agent-deck-notice">{displayText(deckNotice, DISPLAY_LIMITS.message)}</p>}
        <ul
          ref={deckListRef}
          className="new-agent-list"
          role="listbox"
          aria-label="Deck"
          tabIndex={0}
          data-testid="new-agent-deck-list"
          aria-activedescendant={highlighted ? `${titleId}-deck-${choices.indexOf(highlighted)}` : undefined}
          onKeyDown={onDeckKeyDown}
        >
          {choices.map((choice, index) => {
            const disabled = choice.reason !== undefined;
            return (
              <li
                key={choice.deckId}
                id={`${titleId}-deck-${index}`}
                role="option"
                aria-selected={choice.deckId === highlight}
                aria-disabled={disabled || undefined}
                className={`new-agent-row${choice.deckId === highlight ? " is-active" : ""}${disabled ? " is-disabled" : ""}`}
                data-deck-id={choice.deckId}
                onClick={() => {
                  if (disabled) return;
                  setHighlight(choice.deckId);
                  confirmDeck(choice);
                }}
              >
                <span className="new-agent-row-name">{choice.name}</span>
                <span className="new-agent-row-tag">{choice.deckKind}</span>
                {disabled && <span className="new-agent-row-reason">{choice.reason}</span>}
              </li>
            );
          })}
        </ul>
        {choices.length === 0 && <p className="new-agent-hint">No deck is configured.</p>}
      </>
    );
    footer = (
      <>
        <button type="button" className="button secondary" onClick={requestClose}>Cancel</button>
        <button type="button" className="button primary" data-testid="new-agent-deck-next" disabled={!highlighted || highlighted.reason !== undefined} onClick={() => confirmDeck(highlighted)}>Next</button>
      </>
    );
  } else if (step === "directory") {
    const noSubdirectories = listing !== undefined && listing.entries.length === 0;
    body = (
      <>
        <form
          className="new-agent-path"
          onSubmit={(event) => {
            event.preventDefault();
            submitTypedPath();
          }}
        >
          <input
            ref={pathRef}
            aria-label="Path"
            data-testid="new-agent-path"
            value={typedPath}
            placeholder={listingState === "unsupported" ? "Absolute path of a directory on this deck" : "Type a path to go there"}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onChange={(event) => setTypedPath(event.target.value)}
          />
          <button type="submit" className="button secondary compact" disabled={typedPath === ""}>{listingState === "unsupported" ? "Use path" : "Go"}</button>
        </form>
        {listingState === "unsupported" && <p className="new-agent-hint" data-testid="new-agent-no-browse">This deck cannot list directories. Type the directory's absolute path.</p>}
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
              tabIndex={0}
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
            {listing.truncated && <p className="new-agent-hint" data-testid="new-agent-truncated">Not every subdirectory is listed. Type a path to reach one that is not.</p>}
            <p className="new-agent-keys">j/k move · l or Enter opens · h or Backspace goes up · Space uses this directory · / filters</p>
          </>
        )}
      </>
    );
    footer = (
      <>
        <button type="button" className="button secondary" onClick={() => { listingSeq.current += 1; setStep("deck"); setDeck(undefined); }}><ArrowLeft size={14} /> Back</button>
        <button type="button" className="button secondary" onClick={requestClose}>Cancel</button>
        {listing?.parent !== undefined && <button type="button" className="button secondary" onClick={goUp}><ArrowUp size={14} /> Up</button>}
        <button type="button" className="button primary" data-testid="new-agent-use-directory" disabled={!listing} onClick={confirmCurrent}><Check size={14} /> Use this directory</button>
      </>
    );
  } else {
    body = (
      <form
        id={`${titleId}-form`}
        className="new-agent-form"
        data-testid="new-agent-form"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <div className="new-agent-field">
          <span>Dir</span>
          <strong data-testid="new-agent-dir" title={target ? displayText(target.displayPath, DISPLAY_LIMITS.message) : undefined}>{target ? displayText(target.displayPath, DISPLAY_LIMITS.path) : ""}</strong>
        </div>
        <div className="new-agent-field">
          <span id={`${titleId}-mode`}>Mode</span>
          <div className="new-agent-chips" role="group" aria-labelledby={`${titleId}-mode`} data-testid="new-agent-modes" onKeyDown={onModeKeyDown}>
            {modes.map((candidate, index) => (
              <Fragment key={candidate.id}>
                {index === orchestrationEnd && ambiguousChipButtons}
                <button
                  type="button"
                  className={`new-agent-chip${candidate.id === mode ? " is-active" : ""}`}
                  aria-pressed={candidate.id === mode}
                  data-mode={candidate.id}
                  data-testid={`new-agent-mode-${candidate.id}`}
                  disabled={busy}
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
          <select data-testid="new-agent-agent" value={agentChoice} disabled={busy} onChange={(event) => chooseAgent(event.target.value)}>
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
            disabled={busy}
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
            disabled={busy}
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
        {formCleanup && formCleanup.length > 0 && <p className="new-agent-error new-agent-cleanup" role="alert" data-testid="new-agent-cleanup-warning">{cleanupWarning(formCleanup)}</p>}
        {formError && <p className="new-agent-error" role="alert" data-testid="new-agent-error">{displayText(formError, DISPLAY_LIMITS.message)}</p>}
        {formError && displayText(formError, DISPLAY_LIMITS.detail) !== displayText(formError, DISPLAY_LIMITS.message) && (
          <details className="new-agent-detail" data-testid="new-agent-error-detail">
            <summary>Full detail</summary>
            <p>{displayText(formError, DISPLAY_LIMITS.detail)}</p>
          </details>
        )}
        {starting && <p className="new-agent-hint" data-testid="new-agent-starting"><Loader2 className="spin" size={12} /> {STARTING_CLOSE_BLOCKED}</p>}
        {phase === "waiting" && <p className="new-agent-hint" data-testid="new-agent-waiting"><Loader2 className="spin" size={12} /> Started. Waiting for the deck to list it…</p>}
      </form>
    );
    footer = (
      <>
        <button type="button" className="button secondary" disabled={busy} onClick={() => setStep("directory")}><ArrowLeft size={14} /> Back</button>
        <button type="button" className="button secondary" data-testid="new-agent-cancel" disabled={starting} title={starting ? STARTING_CLOSE_BLOCKED : undefined} onClick={requestClose}>Cancel</button>
        <button type="submit" form={`${titleId}-form`} className="button primary" data-testid="new-agent-start" disabled={busy || titleTaken}>
          {phase === "idle" ? <><Plus size={14} /> {selectedOrchestration ? "Start orchestration" : "Start agent"}</> : phase === "starting" ? "Starting…" : "Opening…"}
        </button>
      </>
    );
  }

  return (
    <div className="dialog-backdrop" role="presentation" data-testid="new-agent-backdrop" onMouseDown={requestClose}>
      <section
        className="new-agent-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="new-agent-dialog"
        data-step={step}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={onDialogKeyDown}
      >
        <header>
          <h2 id={titleId}>New agent</h2>
          {deck && <span className="new-agent-deck" data-testid="new-agent-chosen-deck">{deck.name}</span>}
          <button type="button" className="icon-button" aria-label="Close new agent" disabled={starting} title={starting ? STARTING_CLOSE_BLOCKED : undefined} onClick={requestClose}><X size={15} /></button>
        </header>
        <div className="new-agent-body">{body}</div>
        <footer>{footer}</footer>
      </section>
    </div>
  );
}
