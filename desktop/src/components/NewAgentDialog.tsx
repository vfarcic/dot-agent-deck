import { useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { ArrowLeft, ArrowUp, Check, Folder, FolderGit2, Loader2, Plus, X } from "lucide-react";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  deckChoices,
  directoryLabel,
  filterDirectoryEntries,
  fleetLists,
  isDeckGoneError,
  NEW_AGENT_APPEAR_TIMEOUT_MS,
  preselectedDeck,
  seedCommand,
  type DeckChoice,
} from "../lib/newAgent";
import type { DeckDirectoryEntry, DeckDirectoryListing, DeckRuntimeState, NewAgentOption, NewAgentOptions } from "../types";

/**
 * What the dialog needs from the runtime. The two queries are REQUIRED here
 * although they are optional on `DeckRuntimeState`: the overview renders no
 * entry point into this dialog for a runtime without them.
 */
export type NewAgentRuntime = Pick<DeckRuntimeState, "fleet" | "runAction" | "clearError"> & Required<Pick<DeckRuntimeState, "listDirectories" | "newAgentOptions">>;

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
 * The Mode row's chips. One today; PRD #1223 M6 and M7 add one per
 * orchestration the directory defines and the three authoring kinds, which is
 * why this is a list rather than a label.
 */
const MODES = [{ id: "none", label: "No mode" }] as const;

const AUTO_AGENT = "auto";

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
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const commandTouched = useRef(false);
  /** Drops an options reply for a directory the user has since left. */
  const optionsSeq = useRef(0);
  const [formError, setFormError] = useState<string>();
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
    setCommand("");
    commandTouched.current = false;
    setAgentChoice(AUTO_AGENT);
    setOptions(undefined);
    setOptionsError(undefined);
    setFormError(undefined);
    setStep("form");
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
    // Verbatim: the deck canonicalises it, and its reply is what the flow
    // carries. A deck without the listing verb has no reply to give, so the
    // typed path itself goes to the form and the start is where the deck
    // accepts or refuses it.
    if (listingState === "unsupported") {
      confirmDirectory(typedPath, displayText(typedPath, DISPLAY_LIMITS.path));
      return;
    }
    focusListingNext.current = true;
    void loadListing(deck.deckId, typedPath);
  };

  const agents: NewAgentOption[] = options?.kind === "deck" ? options.agents : options?.kind === "unsupported" ? options.desktopAgents : [];

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

  const submit = async () => {
    if (!deck || !target || phase !== "idle") return;
    setFormError(undefined);
    setPhase("starting");
    const agentName = name.trim() ? name : "";
    try {
      const result = await runtime.runAction({
        type: "start_agent",
        deckId: deck.deckId,
        cwd: target.path,
        ...(command.trim() ? { command } : {}),
        ...(agentName ? { displayName: agentName } : {}),
      });
      if (result.agentId === undefined) {
        onNotAppeared({ deckName: deck.name, agentName });
        return;
      }
      setPhase("waiting");
      setAwaiting({ deckId: deck.deckId, agentId: result.agentId, deckName: deck.name, agentName });
    } catch (cause) {
      // The runtime files a failed action under its global error as well; the
      // dialog says it here, beside the values, so that copy is dropped.
      runtime.clearError();
      const message = messageOf(cause);
      if (isDeckGoneError(message)) {
        returnToDeckStep(message);
        return;
      }
      setFormError(message);
      setPhase("idle");
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
        onClose();
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

  const onDialogKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
    }
  };

  const busy = phase !== "idle";
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
        <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
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
        <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
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
          <div className="new-agent-chips" role="group" aria-labelledby={`${titleId}-mode`}>
            {MODES.map((mode) => <button type="button" key={mode.id} className="new-agent-chip is-active" aria-pressed="true" disabled={busy}>{mode.label}</button>)}
          </div>
        </div>
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
            onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => {
              // The TUI form's Enter on Name: move to Command, which submits.
              if (event.key === "Enter") {
                event.preventDefault();
                commandRef.current?.focus();
              }
            }}
          />
        </label>
        <label className="new-agent-field">
          <span>Command</span>
          <input
            ref={commandRef}
            data-testid="new-agent-command"
            value={command}
            disabled={busy}
            placeholder="Empty starts the deck's default shell"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onChange={(event) => {
              commandTouched.current = true;
              setCommand(event.target.value);
            }}
          />
        </label>
        {formError && <p className="new-agent-error" role="alert" data-testid="new-agent-error">{displayText(formError, DISPLAY_LIMITS.message)}</p>}
        {phase === "waiting" && <p className="new-agent-hint" data-testid="new-agent-waiting"><Loader2 className="spin" size={12} /> Started. Waiting for the deck to list it…</p>}
      </form>
    );
    footer = (
      <>
        <button type="button" className="button secondary" disabled={busy} onClick={() => setStep("directory")}><ArrowLeft size={14} /> Back</button>
        <button type="button" className="button secondary" onClick={onClose}>Cancel</button>
        <button type="submit" form={`${titleId}-form`} className="button primary" data-testid="new-agent-start" disabled={busy}>
          {phase === "idle" ? <><Plus size={14} /> Start agent</> : phase === "starting" ? "Starting…" : "Opening…"}
        </button>
      </>
    );
  }

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}>
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
          <button type="button" className="icon-button" aria-label="Close new agent" onClick={onClose}><X size={15} /></button>
        </header>
        <div className="new-agent-body">{body}</div>
        <footer>{footer}</footer>
      </section>
    </div>
  );
}
