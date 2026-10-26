import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createFixtureSnapshot } from "../data/fixture";
import { createDeckBridge, selectRuntimeMode } from "../lib/bridge";
import type { DesktopSettingsDto, EndpointSettingsDto, RemoteEndpointDto, VoiceDictationTargetDto, VoiceDirectoriesDto, VoiceReadingStateDto, VoiceNewAgentDto, VoiceResolvedParamDto, VoiceScreen, VoiceSecretId } from "../lib/bridge";
import type { ReadingSentenceDto, ReadingTarget } from "../lib/reading";
import { voiceDeckStep } from "../lib/newAgent";
import { agentKey, boundAgentKey } from "../lib/agentKey";
import { LaunchCleanupError } from "../lib/actionError";
import { applyTerminalChunk } from "../lib/terminalBuffer";
import { deckName } from "../lib/displayText";
import type { VoiceNumberedListDto } from "../lib/voiceNumbers";
import type { UpgradeChoice, UpgradeEvent } from "../lib/upgrade";
import type { QuestionSelectionDto, QuestionTextSlotDto, VoiceQuestionTarget } from "../lib/voiceQuestion";
const EMPTY_TERMINAL_DATA: Record<string, TerminalBuffer> = {};
import type { AgentTarget, CleanupWarningEntry, DeckAction, DeckFleet, DeckListingOptions, DeckRuntimeState, DeckSnapshot, DesktopFeatures, InboxActionVerb, RuntimeMode, TerminalBuffer } from "../types";

/**
 * The snapshot a runtime starts with, before any deck has answered. Lifted out
 * of `useState` when PRD #742 M4 made the state a fleet — the seed is one deck
 * either way, because there is exactly one deck the app knows of before a
 * handshake: the one the selection resolves to.
 */
function seedSnapshot(mode: RuntimeMode): DeckSnapshot {
  if (mode === "live") {
    // PR #416 review B2: live mode gets its own HONEST empty seed. The old
    // seed spread createFixtureSnapshot("empty"), which empties the arrays
    // but keeps the scalars — so with no daemon running the top bar showed a
    // fixture branch, run id, elapsed time and node count as steady state,
    // with no DEMO DATA banner to disclaim them. Nothing here is invented:
    // every field says "unavailable" until the daemon says otherwise.
    const fixtureShape = createFixtureSnapshot("empty");
    return {
      ...fixtureShape,
      runId: "—",
      repo: "No active project",
      // PRD #745 M8: no branch and no attempt at all, rather than a
      // placeholder branch and a zeroed attempt counter. Neither exists
      // daemon-side, and the seed is what the topbar shows before the first
      // snapshot arrives.
      branch: undefined,
      worktree: "No active project",
      elapsed: "—",
      spend: 0,
      currentNode: 0,
      totalNodes: 0,
      currentAttempt: undefined,
      stages: [],
      agents: [],
      evidence: [],
      handoffs: [],
      connection: { status: "loading", message: "Connecting to the local daemon…" },
    };
  }
  const initial = createFixtureSnapshot("empty");
  return {
    ...initial,
    connection: { status: "loading", message: "Loading deterministic fixture…" },
  };
}

/**
 * Issue #1845 — whether a `boundAgentKey` names an agent that a CONNECTED deck
 * in `fleet` no longer lists: one it closed, or one of a process it restarted
 * away from. Keys of a deck that is not connected, or not in the fleet at all,
 * are never retired here. Matched by the deck's `agentKey` prefix rather than
 * by splitting the key, which `NUL` makes unambiguous.
 */
function retiredAgentKeys(fleet: DeckFleet): (key: string) => boolean {
  const listed = new Set<string>();
  const decks: string[] = [];
  for (const deck of fleet) {
    const deckId = deck.connection.deckId;
    if (deck.connection.status !== "connected" || deckId === undefined) continue;
    decks.push(agentKey(deckId, ""));
    for (const agent of deck.agents) listed.add(boundAgentKey({ deckId, agentId: agent.id, incarnation: agent.incarnation }));
  }
  return (key) => !listed.has(key) && decks.some((prefix) => key.startsWith(prefix));
}

export function useDeckRuntime(): DeckRuntimeState {
  const mode = useMemo(selectRuntimeMode, []);
  const bridge = useMemo(() => createDeckBridge(mode), [mode]);
  /**
   * The whole fleet, selected deck first (PRD #742 M4). It replaced a single
   * `snapshot` because the desktop crate now runs one watcher per observed
   * deck: N snapshots arrive per coalescing window, and a single-snapshot state
   * renders whichever landed last. `snapshot` below is `fleet[0]`, so every
   * single-deck screen reads exactly what it read before.
   */
  const [fleet, setFleet] = useState<DeckFleet>(() => [seedSnapshot(mode)]);
  const snapshot = fleet[0];
  /**
   * The deck every per-agent map below is keyed against when the producer of a
   * value could not name one itself.
   *
   * A ref rather than a dependency: `runAction`'s verdict and the geometry
   * subscription are both callbacks that must not be rebuilt on every snapshot,
   * and what they need is the deck in force at the instant they fire rather
   * than the one that was in force when they were created.
   */
  const selectedDeckIdRef = useRef<string | undefined>(snapshot.connection.deckId);
  selectedDeckIdRef.current = snapshot.connection.deckId;
  /* PRD #1223 — the fleet as of the last render, so a declaration made from a
     callback describes the deck step the New agent dialog would show NOW, and
     a cleanup warning (issue #1234) names the deck its action was sent to. */
  const fleetRef = useRef(fleet);
  fleetRef.current = fleet;
  /**
   * The latest reported failure's sentence, or nothing.
   *
   * PRD #742 M8 carried a `{ message, id }` here so `App` could suppress one
   * dismissed failure by id rather than by sentence. Issue #1046 landed on
   * `main` while that was in flight and made the dismissal CLEAR this instead
   * (see {@link clearError}), which answers the same question with less: a
   * cleared error is per-occurrence by construction, so a second failure
   * carrying an identical sentence sets it again and shows.
   *
   * One slot, and every writer replaces it: the next failure, the next action
   * and `reconnect()` all do. That is right for a sentence and wrong for a
   * safety warning, which is why the roles a failed launch could not confirm
   * are stopped are NOT held here — see `cleanupWarnings` below.
   */
  const [error, setError] = useState<string>();
  /**
   * Issue #1234 — every unconfirmed-stop warning the user has not dismissed.
   *
   * PRD #1223 audit V7 put these roles in the failure slot beside the sentence,
   * first as a second `useState` and then, after audit W1 found a later
   * rejection could inherit an earlier one's roles, as one atomic value with
   * it. That made the pairing honest and kept the lifetime wrong: with two
   * actions in flight, a `LaunchCleanupError` and an ordinary rejection
   * straight after it land in one React batch, the second replaces the whole
   * failure, and no frame ever names roles that may still be running. Starting
   * any action and `reconnect()` cleared them the same way.
   *
   * So they get a lifetime of their own. A rejection APPENDS an entry, only
   * `dismissCleanupWarning` removes one, and the id is minted per rejection so
   * a dismissal aimed at one entry cannot take a later one naming the same
   * roles with it.
   */
  const [cleanupWarnings, setCleanupWarnings] = useState<readonly CleanupWarningEntry[]>([]);
  const nextCleanupWarningId = useRef(0);
  // PTY bytes deliberately bypass React state. Routing every output chunk
  // through setState re-rendered the whole deck per chunk per agent — with six
  // streaming agents the main thread spent its time reconciling instead of
  // letting xterm scroll. Buffers live in a ref; terminals subscribe directly.
  //
  // Both are keyed by `agentKey(deckId, agentId)` since PRD #1105's security
  // audit. Leaving a deck detaches its sessions but clears no buffer, so a
  // bare-id map handed the next deck's same-id agent up to a megabyte of the
  // previous deck's output — written straight into the new xterm by
  // `TerminalViewport`'s `!previous` branch, under a correctly resolved
  // heading, with the live transcript empty and nothing on screen saying so.
  //
  // And by the daemon PROCESS too, `boundAgentKey`, since PRD #1666 part 2
  // (G-fix7): a restarted deck numbers its agents from 1 again, so a
  // `(deckId, agentId)` buffer handed a freshly opened successor the previous
  // process's retained output — under the successor's heading, for as long as
  // its own attach was pending or had failed.
  const terminalBuffersRef = useRef<Record<string, TerminalBuffer>>({});
  const terminalListenersRef = useRef<Map<string, Set<(buffer: TerminalBuffer) => void>>>(new Map());

  const terminalFeed = useMemo(() => ({
    get: (deckId: string | undefined, agentId: string, incarnation: string | undefined) => terminalBuffersRef.current[boundAgentKey({ deckId, agentId, incarnation })],
    subscribe: (deckId: string | undefined, agentId: string, incarnation: string | undefined, listener: (buffer: TerminalBuffer) => void) => {
      const key = boundAgentKey({ deckId, agentId, incarnation });
      const listeners = terminalListenersRef.current.get(key) ?? new Set();
      listeners.add(listener);
      terminalListenersRef.current.set(key, listeners);
      return () => {
        listeners.delete(listener);
        // The last terminal on an agent leaving takes the agent's empty set
        // with it (issue #1845), unless a newer subscription already replaced it.
        if (!listeners.size && terminalListenersRef.current.get(key) === listeners) terminalListenersRef.current.delete(key);
      };
    },
  }), []);

  const updateTerminal = useCallback((event: Parameters<typeof applyTerminalChunk>[1]) => {
    const data = event.data.byteLength
      ? event.data
      : event.message
        ? new TextEncoder().encode(`\r\n[terminal] ${event.message}\r\n`)
        : event.data;
    // The producer names the deck; a producer that cannot falls back to the one
    // this runtime currently believes is selected, which is what a bare-id
    // producer implicitly meant. Never the other way round — the bridge's own
    // notion of the selection is a snapshot ahead of React's.
    const key = boundAgentKey({ deckId: event.deckId ?? selectedDeckIdRef.current, agentId: event.agentId, incarnation: event.incarnation });
    const current = terminalBuffersRef.current[key];
    const next = applyTerminalChunk(current, { ...event, data });
    if (next === current) return;
    terminalBuffersRef.current[key] = next;
    for (const listener of terminalListenersRef.current.get(key) ?? []) listener(next);
  }, []);

  /**
   * Replace the SELECTED deck and leave the rest of the fleet where it is.
   *
   * Every failure this hook reports is about the connection the app itself
   * owns — the bootstrap it just made — and that is the selected deck's. A
   * remote deck that is down reports through its own `ConnectionView` in its
   * own group, which is the whole point of the per-deck state; rewriting the
   * fleet here would paint every deck with one deck's failure.
   */
  const updateSelected = useCallback((update: (current: DeckSnapshot) => DeckSnapshot) => {
    setFleet((current) => [update(current[0]), ...current.slice(1)]);
  }, []);

  /**
   * Take a fleet from the bridge, and never an empty one.
   *
   * `DeckFleet` says it is never empty and both bridges honour that, but the
   * type cannot express it — and the cost of being wrong is not a bad render,
   * it is `fleet[0]` being `undefined` under every single-deck screen. Keeping
   * the previous fleet is the honest fallback: it is the last thing that was
   * actually true, and the next snapshot replaces it.
   */
  const adoptFleet = useCallback((next: DeckFleet) => {
    if (!next.length) return;
    setFleet(next);
  }, []);

  const reconnect = useCallback(async () => {
    setError(undefined);
    updateSelected((current) => ({ ...current, connection: { ...current.connection, status: "loading", message: "Reconnecting…" } }));
    try {
      const connected = await bridge.connect();
      adoptFleet(connected);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      updateSelected((current) => ({
        ...current,
        health: "failed",
        connection: { status: "error", message },
      }));
    }
  }, [adoptFleet, bridge, updateSelected]);

  useEffect(() => {
    let active = true;
    let unsubscribe: (() => void) | undefined;

    void (async () => {
      try {
        unsubscribe = await bridge.subscribe(
          (next) => active && adoptFleet(next),
          (event) => active && updateTerminal(event),
        );
        if (!active) {
          unsubscribe();
          return;
        }
        const initial = await bridge.connect();
        if (active) adoptFleet(initial);
      } catch (cause) {
        if (!active) return;
        const message = cause instanceof Error ? cause.message : String(cause);
        setError(message);
        updateSelected((current) => ({ ...current, health: "failed", connection: { status: "error", message } }));
      }
    })();

    return () => {
      active = false;
      unsubscribe?.();
      void bridge.dispose();
    };
  }, [adoptFleet, bridge, updateSelected, updateTerminal]);

  const runAction = useCallback(async (action: DeckAction) => {
    setError(undefined);
    const sentToDeckId = selectedDeckIdRef.current;
    try {
      const result = await bridge.runAction(action);
      return result;
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      // Issue #1234: the roles as data (audit V7), in a queue of their own, so
      // a later rejection replacing the sentence above cannot take them too.
      if (cause instanceof LaunchCleanupError) {
        nextCleanupWarningId.current += 1;
        // A deck-scoped action names its deck; every other one went to the
        // selection as it was when the action was sent.
        const targetDeckId = "deckId" in action && typeof action.deckId === "string" ? action.deckId : sentToDeckId;
        const target = fleetRef.current.find((deck) => deck.connection.deckId === targetDeckId);
        const entry: CleanupWarningEntry = { id: nextCleanupWarningId.current, stops: cause.unconfirmedStops, ...(target ? { deck: deckName(target.connection) } : {}) };
        setCleanupWarnings((current) => [...current, entry]);
      }
      throw cause;
    }
  }, [bridge]);

  /*
   * Issue #1046: the toast in `App.tsx` renders on `notice || error`, and its
   * dismiss button could reach `notice` and nothing else — so an error-sourced
   * message survived the click and the X read as dead. What the button now does
   * with the two halves is decided there. Clearing is safe because `error` is transient
   * per-action state, not connection state: `runAction` and `reconnect` already
   * clear it at the start of each attempt, and what a failed connection leaves
   * behind for the banner is `snapshot.connection`, which this does not touch.
   */
  const clearError = useCallback(() => {
    setError(undefined);
  }, []);

  /* Issue #1234 — the one writer that removes a cleanup warning. */
  const dismissCleanupWarning = useCallback((id: number) => {
    setCleanupWarnings((current) => (current.some((entry) => entry.id === id) ? current.filter((entry) => entry.id !== id) : current));
  }, []);

  const getSettings = useCallback(() => bridge.getSettings(), [bridge]);
  // Stable for the lifetime of the bridge: `useZoom` holds it across a
  // capture-phase listener whose effect must not be torn down and re-registered
  // on every render.
  const setZoom = useCallback((level: number) => bridge.setZoom(level), [bridge]);
  const saveSettings = useCallback(
    (settings: DesktopSettingsDto, base?: DesktopSettingsDto) => bridge.saveSettings(settings, base),
    [bridge],
  );
  // PRD #741 M10. Not wrapped in the `setError` bookkeeping `runAction` uses,
  // for the same reason `listProjects` is not: every outcome here is a
  // classified report the panel renders in place, and routing an unreachable
  // deck into the deck's global error toast would present a settings answer as
  // a fault of the screen behind it.
  const testEndpoint = useCallback(
    (settings: DesktopSettingsDto, selection: string) => bridge.testEndpoint(settings, selection),
    [bridge],
  );

  // PRD #802 M4. Not wrapped in the `setError` bookkeeping `runAction` uses,
  // for `testEndpoint`'s reason: every outcome here is something the settings
  // panel renders in place, and routing "your keychain is locked" into the
  // deck's global error toast would present a settings answer as a fault of
  // the screen behind it.
  const secretStatus = useCallback((id: VoiceSecretId) => bridge.secretStatus(id), [bridge]);
  const storeSecret = useCallback((id: VoiceSecretId, secret: string) => bridge.storeSecret(id, secret), [bridge]);
  const forgetSecret = useCallback((id: VoiceSecretId) => bridge.forgetSecret(id), [bridge]);
  // Issue #1426. Not wrapped in `setError` either: a refused name is a sentence
  // the Daemons panel shows beside the field.
  const renameDeck = useCallback((deck: RemoteEndpointDto, name: string) => bridge.renameDeck(deck, name), [bridge]);
  const defaultDeckName = useCallback((host: string, user?: string) => bridge.defaultDeckName(host, user), [bridge]);
  const checkDeckName = useCallback((name: string, id?: string) => bridge.checkDeckName(name, id), [bridge]);

  /*
   * PRD #802 M6: the voice seam. Every one is wrapped so its identity is stable
   * for the lifetime of the bridge, which is load-bearing rather than tidy here
   * — `useDeckRuntime` returns a fresh object on every render, and the panel
   * polls `voiceStatus` from an effect and cancels from another. An identity
   * that changed per render would restart that poll on every commit.
   *
   * Not wrapped in the `setError` bookkeeping `runAction` uses, for
   * `secretStatus`' reason: every outcome here is a sentence the panel renders
   * in place — including every refusal, which is what an outcome IS — and
   * routing "no matching action" into the deck's global error toast would
   * present a voice answer as a fault of the screen behind it.
   */
  /* Every declaration carries the New agent dialog's deck step, computed from
     the same `fleet` the dialog reads, so what voice says it preselected and
     what the dialog preselects are judged against one list. */
  const declareVoiceScreen = useCallback((screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto, endpoints?: EndpointSettingsDto, dictation?: VoiceDictationTargetDto, reading?: VoiceReadingStateDto) => bridge.declareVoiceScreen(screen, directories, newAgent, voiceDeckStep(fleetRef.current), endpoints, dictation, reading), [bridge]);
  const resolveVoice = useCallback((utterance: string) => bridge.resolveVoice(utterance), [bridge]);
  const answerVoiceChoice = useCallback((utterance: string, action: string, offered: VoiceResolvedParamDto[]) => bridge.answerVoiceChoice(utterance, action, offered), [bridge]);
  const answerVoiceNumber = useCallback((utterance: string, heard: VoiceNumberedListDto, generation: number) => bridge.answerVoiceNumber(utterance, heard, generation), [bridge]);
  const resolveVoiceQuestion = useCallback((target: VoiceQuestionTarget, utterance: string, form: QuestionSelectionDto[], awaitingText?: QuestionTextSlotDto, localOnly?: boolean) => bridge.resolveVoiceQuestion(target, utterance, form, awaitingText, localOnly), [bridge]);
  const sendVoiceAnswer = useCallback((target: VoiceQuestionTarget, form: QuestionSelectionDto[], confirmedAlways: boolean, lease: string) => bridge.sendVoiceAnswer(target, form, confirmedAlways, lease), [bridge]);
  const cancelVoiceAnswer = useCallback((lease: string) => bridge.cancelVoiceAnswer(lease), [bridge]);
  const voiceCommands = useCallback((screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto) => bridge.voiceCommands(screen, directories, newAgent), [bridge]);
  const voiceStart = useCallback(() => bridge.voiceStart(), [bridge]);
  const voiceStop = useCallback((onSplit?: () => void) => bridge.voiceStop(onSplit), [bridge]);
  const voiceStatus = useCallback(() => bridge.voiceStatus(), [bridge]);
  const voiceCancel = useCallback(() => bridge.voiceCancel(), [bridge]);
  // PRD #1497 — reading's speech and its turn sentences.
  const voiceSpeechPlan = useCallback(() => bridge.voiceSpeechPlan(), [bridge]);
  const voiceSpeechAudio = useCallback((text: string) => bridge.voiceSpeechAudio(text), [bridge]);
  const voiceReadingStart = useCallback((target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => bridge.voiceReadingStart(target, onSentence), [bridge]);
  const voiceReadingStop = useCallback((session: number) => bridge.voiceReadingStop(session), [bridge]);
  const onVoiceReadingConsentOff = useCallback((listener: () => void) => bridge.onVoiceReadingConsentOff(listener), [bridge]);
  const onVoiceReadingConsentOn = useCallback((listener: () => void) => bridge.onVoiceReadingConsentOn(listener), [bridge]);

  const sendTerminalInput = useCallback((target: AgentTarget, data: string, precondition?: () => boolean) => bridge.sendTerminalInput(target, data, precondition), [bridge]);
  const resizeTerminal = useCallback((target: AgentTarget, cols: number, rows: number) => bridge.resizeTerminal(target, cols, rows), [bridge]);
  // Stable for the lifetime of the bridge, because the screens declare their
  // shown set from an effect: an identity that changed every render would fire
  // that effect every render (PRD #745 M7).
  const setShownTerminals = useCallback((targets: AgentTarget[]) => bridge.setShownTerminals(targets), [bridge]);
  // PRD #819 M6. Deliberately NOT wrapped in the `setError` bookkeeping
  // `runAction` uses: an empty listing and an unresolvable path are ordinary
  // outcomes of choosing a project, and routing them into the deck's global
  // error toast would present the first-run state as a fault. The picker owns
  // its own state and says what it means.
  const listProjects = useCallback(() => bridge.listProjects(), [bridge]);
  const resolveProject = useCallback((path: string) => bridge.resolveProject(path), [bridge]);
  // PRD #1223 M4, and for the same reason: a deck that cannot list, a path it
  // refuses and a deck that left the fleet are all things the New agent dialog
  // says in place, not faults of the screen behind it.
  const listDirectories = useCallback((deckId: string, path?: string, options?: DeckListingOptions) => (options ? bridge.listDirectories(deckId, path, options) : bridge.listDirectories(deckId, path)), [bridge]);
  const newAgentOptions = useCallback((deckId: string) => bridge.newAgentOptions(deckId), [bridge]);
  const newAgentOrchestrations = useCallback((deckId: string, path: string) => bridge.newAgentOrchestrations(deckId, path), [bridge]);
  // PRD #1487 M5: Upgrade and Replace daemon. Their outcome — kept running,
  // failed while installing — is the dialog's to show, not the global error
  // toast's, so these bypass `runAction` too. A bridge without the verbs (some
  // test doubles) leaves both absent, and the screens then offer no button.
  const upgradeDaemon = useMemo(() => (typeof bridge.upgradeDaemon === "function"
    ? (deckId: string, onEvent: (event: UpgradeEvent) => void) => bridge.upgradeDaemon(deckId, onEvent)
    : undefined), [bridge]);
  // PRD #1666 §6.4: Retry and Cancel on a listed deck message. The answer is
  // the row's to show, so this bypasses `runAction` too.
  const inboxAction = useMemo(() => (typeof bridge.inboxAction === "function"
    ? (target: AgentTarget, item: { paneId: string; messageId: string; revision: number }, verb: InboxActionVerb) =>
      bridge.inboxAction!(target, item, verb)
    : undefined), [bridge]);
  const decideUpgrade = useMemo(() => (typeof bridge.decideUpgrade === "function"
    ? (upgradeId: string, questionId: number, choice: UpgradeChoice) => bridge.decideUpgrade(upgradeId, questionId, choice)
    : undefined), [bridge]);

  // PRD #882: the geometry the daemon has applied per agent. Held here rather
  // than inside each tile because the push is per agent and arrives on one
  // bridge-wide subscription — and because a tile has to be able to read a
  // value that was pushed before it mounted (another client can constrain an
  // agent long before anyone opens a terminal on it here).
  //
  // Keyed by `agentKey(deckId, agentId)` since PRD #1105's security audit: no
  // per-agent eviction removes an entry, so a bare-id map handed the pane for
  // deck B's `planner` deck A's grid, and an attach that beat the pane's first
  // fit submitted A's cached dimensions to B's PTY.
  //
  // And by `boundAgentKey` since PRD #1666 part 2 (G-fix7), for the buffers'
  // reason: a restarted deck's agent with the same id is not the agent whose
  // grid this cached.
  const [appliedGeometry, setAppliedGeometry] = useState<Record<string, { rows: number; cols: number }>>({});
  useEffect(() => {
    return bridge.onTerminalGeometry((agentId, rows, cols, deckId, incarnation) => {
      const key = boundAgentKey({ deckId: deckId ?? selectedDeckIdRef.current, agentId, incarnation });
      setAppliedGeometry((current) => {
        const existing = current[key];
        if (existing && existing.rows === rows && existing.cols === cols) return current;
        return { ...current, [key]: { rows, cols } };
      });
    });
  }, [bridge]);

  // Issue #1845: the buffers and grids above are kept per agent and per daemon
  // process, and nothing else removes one — so every agent a deck closed, and
  // every agent of a process the deck restarted away from, held up to a
  // megabyte of output until the window reloaded. Forget an agent once a deck
  // that is connected stops listing it. A deck that is not answering lists
  // nobody and vouches for nothing, so its agents are kept until it answers.
  useEffect(() => {
    const retired = retiredAgentKeys(fleet);
    for (const key of Object.keys(terminalBuffersRef.current)) {
      if (retired(key)) delete terminalBuffersRef.current[key];
    }
    setAppliedGeometry((current) => {
      const entries = Object.entries(current);
      const kept = entries.filter(([key]) => !retired(key));
      return kept.length === entries.length ? current : Object.fromEntries(kept);
    });
  }, [fleet]);

  // Issue #1198: the app's experimental surfaces, asked ONCE per bridge. The
  // flag is a presentation switch read at startup — changing it means
  // restarting the app — so there is no re-query to race a navigation with.
  // A missing method or a refused call leaves this `undefined`, which every
  // reader takes as all hidden (`desktopFeaturesOf`).
  const [desktopFeatures, setDesktopFeatures] = useState<DesktopFeatures>();
  useEffect(() => {
    let cancelled = false;
    const ask = typeof bridge.desktopFeatures === "function" ? bridge.desktopFeatures() : undefined;
    void ask?.then((features) => { if (!cancelled) setDesktopFeatures(features); }, () => undefined);
    return () => { cancelled = true; };
  }, [bridge]);

  return {
    appliedGeometry,
    desktopFeatures,
    mode,
    snapshot,
    fleet,
    terminalData: EMPTY_TERMINAL_DATA,
    terminalFeed,
    error,
    cleanupWarnings,
    dismissCleanupWarning,
    clearError,
    runAction,
    ...(upgradeDaemon ? { upgradeDaemon } : {}),
    ...(decideUpgrade ? { decideUpgrade } : {}),
    ...(inboxAction ? { inboxAction } : {}),
    sendTerminalInput,
    resizeTerminal,
    setShownTerminals,
    reconnect,
    listProjects,
    resolveProject,
    listDirectories,
    newAgentOptions,
    newAgentOrchestrations,
    getSettings,
    saveSettings,
    testEndpoint,
    secretStatus,
    storeSecret,
    forgetSecret,
    renameDeck,
    defaultDeckName,
    checkDeckName,
    declareVoiceScreen,
    resolveVoice,
    answerVoiceChoice,
    answerVoiceNumber,
    resolveVoiceQuestion,
    sendVoiceAnswer,
    cancelVoiceAnswer,
    voiceCommands,
    voiceStart,
    voiceStop,
    voiceStatus,
    voiceCancel,
    voiceSpeechPlan,
    voiceSpeechAudio,
    voiceReadingStart,
    voiceReadingStop,
    onVoiceReadingConsentOff,
    onVoiceReadingConsentOn,
    setZoom,
  };
}
