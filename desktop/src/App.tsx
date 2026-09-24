import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  AlertTriangle,
  ArrowRight,
  Blocks,
  BookMarked,
  Bot,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleStop,
  Command,
  FolderGit2,
  Gauge,
  GitBranch,
  HelpCircle,
  History,
  Keyboard,
  LayoutList,
  Network,
  PanelRight,
  Pause,
  Play,
  RefreshCw,
  Search,
  Send,
  Settings2,
  ShieldAlert,
  SlidersHorizontal,
  Sparkles,
  SquareTerminal,
  X,
  Zap,
} from "lucide-react";
import { AgentOverview } from "./components/AgentOverview";
import { AgentTile, type AgentTileProps } from "./components/AgentTile";
import { ConfirmDialog, type ConfirmState } from "./components/ConfirmDialog";
import { DeckSelector } from "./components/DeckSelector";
import { HandoffRail } from "./components/HandoffRail";
import { ProfilesPanel, ProjectsPanel, PromptLibraryPanel, WorkflowPanel } from "./components/ConfigurationPanels";
import { SettingsSheet } from "./components/SettingsSheet";
import { VoiceControlPanel } from "./components/VoiceControlPanel";
import { SettingsBridgeProvider } from "./lib/settingsBridge";
import { DISPLAY_LIMITS, deckName, displayActivity, displayText } from "./lib/displayText";
import { useAgentProfiles } from "./hooks/useAgentProfiles";
import { useDeckRuntime } from "./hooks/useDeckRuntime";
import { useDaemonProjects } from "./hooks/useDaemonProjects";
import { usePromptLibrary } from "./hooks/usePromptLibrary";
import { useDesktopSettings, type DesktopSettingsState } from "./hooks/useDesktopSettings";
import { useInertBackground } from "./hooks/useInertBackground";
import { useShownTerminals } from "./hooks/useShownTerminals";
import { useHeldAgentRecord, type HeldAgentRecord } from "./hooks/useHeldAgentRecord";
import { useZoom } from "./hooks/useZoom";
import { agentKey } from "./lib/agentKey";
import { VOICE_ACTIONS, dispatchVoiceAction, type DeckOverlay, type NewAgentVoice, type VoiceContextChannel, type VoiceDispatchContext, type VoiceDispatchTarget, type VoiceOverviewContext, type VoicePanelContext, type VoiceScreenContext } from "./lib/voiceActions";
import { unreachableDeckTerminalState } from "./lib/terminalInput";
import { applyAppearance } from "./lib/appearance";
import { desktopWorkflowPlatformIssue } from "./lib/platform";
import { LaunchCleanupError } from "./lib/actionError";
import { CleanupWarning } from "./components/CleanupWarning";
import type { VoiceDirectoriesDto, VoiceNewAgentDto, VoiceOutcomeDto } from "./lib/bridge";
import type { AgentSession, DeckAction, DeckRuntimeState, DeckSnapshot, DeckView, EvidenceItem, PanelTab, WorkflowLaunchConfig } from "./types";
import { modeScopedKey } from "./lib/bridge";

const WORKFLOW_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.workflow-preview.v1");
/**
 * The daemon's stable refusal codes this screen recognises, matched as CODES
 * rather than as prose. Each is the first token of `AttachResponse.error`,
 * followed by `": "` — and the sentence after it is deliberately uninformative
 * for a path the daemon does not already know, so this screen must not depend
 * on how much it said.
 *
 * `unresolved` (`daemon_protocol::PROJECT_ERR_UNRESOLVED`): the path did not
 * resolve. `stale-revision` (`PROJECT_ERR_STALE_REVISION`): it did, but against
 * different config bytes than the picker read — the TOCTOU gate M4 added, whose
 * remedy is to resolve again.
 *
 * The last three are token-time refusals, all of which mean "nothing was
 * started" and two of which arrive from the PRD #819 audit fix:
 *
 * - `stale-token` (`PROJECT_ERR_STALE_TOKEN`): the preparation is unknown to
 *   this daemon or has aged out of its TTL.
 * - `stale-preparation` (`PROJECT_ERR_STALE_PREPARATION`): the preparation was
 *   ours and live, but the project, its config or the published context moved
 *   under it — most ordinarily because a second launch in the same project
 *   replaced the context at its fixed path. The remedy is a real one the user
 *   can take: prepare again, which is what re-resolving and re-launching does.
 * - `unsupported-platform` (`PROJECT_ERR_UNSUPPORTED_PLATFORM`): the daemon
 *   will not publish a coordinator context on its platform, because the
 *   owner-only guarantee the publish documents is Unix-only. No amount of
 *   retrying helps, so this one says so instead of inviting one.
 *
 * `preparation-mismatch` (`PROJECT_ERR_PREPARATION_MISMATCH`, PRD #819 Greptile
 * P1(a)) is deliberately NOT in that list and falls through to the generic
 * notice carrying the daemon's own sentence. It means this app submitted a
 * project, workflow or role other than the one the daemon prepared, which is a
 * defect in this client rather than a state the user can be walked out of —
 * re-preparing and sending the same thing again earns the same refusal, so
 * offering that as a remedy would be a lie. Nothing was started either way.
 */
const PROJECT_UNRESOLVED_CODE = "unresolved: ";
const PROJECT_STALE_REVISION_CODE = "stale-revision: ";
const PROJECT_STALE_TOKEN_CODE = "stale-token: ";
const PROJECT_STALE_PREPARATION_CODE = "stale-preparation: ";
const PROJECT_UNSUPPORTED_PLATFORM_CODE = "unsupported-platform: ";
const DESKTOP_EVIDENCE_QUERY = "(min-width: 1260px)";

function evidenceOpenOnFirstLoad(): boolean {
  if (typeof window === "undefined") return true;
  if (typeof window.matchMedia === "function") return window.matchMedia(DESKTOP_EVIDENCE_QUERY).matches;
  return window.innerWidth >= 1260;
}

export default function App() {
  return <DeckShell runtime={useDeckRuntime()} />;
}

/**
 * Owns which top-level surface is mounted. The overview renders *instead of*
 * the deck, so the state belongs above `ControlDeck` rather than as one more
 * boolean inside it — none of the deck's own `useState` booleans is a view, and
 * five of the rail's seven buttons are overlay toggles over an always-mounted
 * deck. `DeckView` is a discriminated union from the start so PRD #745
 * iteration 3's group and single-agent views arrive as added variants.
 *
 * The deck stays the default: launching the app lands exactly where it does
 * today.
 *
 * PRD #1105 M2 narrowed the first sentence rather than repealing it. The two
 * SCREEN variants still replace one another; the `"agent"` variant does not —
 * it names the screen to keep mounted underneath and renders the pane over it,
 * which is why the switch below reads `base` rather than `view.kind`.
 */
export function DeckShell({ runtime, workflowPlatformIssue, initialView = { kind: "deck" } }: { runtime: DeckRuntimeState; workflowPlatformIssue?: string; initialView?: DeckView }) {
  const [view, setView] = useState<DeckView>(initialView);
  /**
   * The settings document and the zoom keys live HERE, not in the deck,
   * because both are the app's and not one screen's — and `DeckShell` is the
   * only component mounted for every view. The deck is unmounted while the
   * overview is up, which is exactly how the zoom keys came to be dead on the
   * overview: they were bound in `ControlDeck`, so they went away with it.
   * `zooms on the overview, not only on the deck` in
   * `components/AgentOverview.test.tsx` is the guard, with the deck case
   * beside it as its control.
   *
   * A second view added later gets zoom for free. One that renders its own
   * settings state instead of taking this one would re-create the bug.
   */
  const settings = useDesktopSettings(runtime);
  useZoom(runtime, settings);
  /**
   * PRD #802 M7 — where the mounted deck publishes the context it can serve.
   *
   * A `useRef` and not state: nothing renders from it, and the only reader is
   * `dispatchVoice` below, at the moment a command runs. Making it state would
   * re-render the whole shell on every commit of the deck beneath it, to serve
   * a value nothing displays.
   */
  const deckVoiceContext = useRef<VoiceScreenContext | undefined>(undefined);
  /**
   * PRD #802 — the VOICE SURFACE's own half of the dispatch context.
   *
   * The deck publishes upward through `deckVoiceContext` while it is mounted,
   * which is what makes a deck-only row servable. The voice surface is mounted
   * on **every** screen — it is this component's second child, beside the screen
   * switch — so what it publishes here is servable everywhere, which is exactly
   * what `voice_off` needs and what neither the deck nor this shell can offer.
   */
  const panelVoiceContext = useRef<VoicePanelContext | undefined>(undefined);
  /**
   * PRD #1223 U5 — the OVERVIEW's half, published while it is mounted:
   * `closeNewAgent` while the New agent dialog is open, so `close` can close
   * that dialog rather than report "nothing to close"; `openNewAgent` while it
   * is closed; and the directory browser's three moves.
   */
  const overviewVoiceContext = useRef<Partial<VoiceOverviewContext> | undefined>(undefined);
  /**
   * PRD #1223 — the New agent dialog's own slot: what its directory browser
   * shows, and its three moves. Created here rather than in the overview
   * because the voice surface, which declares the browser with each
   * utterance, is this shell's child and not the overview's; the overview
   * hands the slot to the dialog and serves the moves by reading it.
   */
  const newAgentVoice = useRef<NewAgentVoice | undefined>(undefined);
  const agentView = view.kind === "agent" ? view : undefined;
  /**
   * Back, and the whole of it. The destination is read off the view rather
   * than popped from a stack, so an agent view that was never navigated TO —
   * the app's `initialView`, a future deep link — closes to a real screen
   * instead of to nothing.
   */
  const closeAgent = useCallback(() => setView((current) => (current.kind === "agent" ? { kind: current.from } : current)), []);
  /**
   * PRD #802 M2 — the same close, dispatched through the action registry.
   *
   * `closeAgent` itself stays, and the split is deliberate: the two effects
   * below close the view because its SUBJECT has gone (the selected deck moved,
   * the daemon ended the agent), which is an invariant the app maintains rather
   * than an action anybody asked for. The registry is the dispatch seam for
   * things a user — or voice — does, and closing a view nobody asked to close
   * is not one of them.
   *
   * The deck's own Close button is NOT routed here: {@link DeckSurface} builds
   * its own context and dispatches there, so routing it here as well would put
   * two dispatches on one click.
   */
  const closeAgentView = useCallback(() => VOICE_ACTIONS.closeAgentView.run({ closeAgentView: closeAgent }), [closeAgent]);
  /**
   * `Escape`, bound at `window` because the pane has no single focusable owner
   * — focus is usually inside xterm's helper textarea, which swallows keys
   * before React sees them.
   *
   * Exactly ONE `window` `keydown` listener exists for the pane, and that is a
   * property of the pane's contents rather than of this line: `OutputReader`
   * binds one too, `stopPropagation` does nothing between two listeners on the
   * same target, and the order is registration order.
   *
   * It takes TWO gates in `AgentTile` to make that true, and the PR review
   * found the second missing. `presentation="overlay"` renders no Reader, which
   * covers the tile being promoted; the other tiles the pane is drawn OVER stay
   * at `"tile"` and kept any Reader they already had, so opening the Reader on
   * one tile and the pane on another left two listeners answering one
   * `Escape` — closing the pane and that Reader together. `panePresent` is the
   * screen-wide gate that closes it, and it dismisses rather than hides: a
   * Reader restored on close would answer the next `Escape` instead of the
   * deck.
   */
  useEffect(() => {
    if (!agentView) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") closeAgentView();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [agentView, closeAgentView]);
  const base = agentView?.from ?? view.kind;
  const selectedDeckId = runtime.snapshot.connection.deckId;
  /** The fleet entry the open pane's agent lives on, if the app is observing it. */
  const paneDeck = agentView ? runtime.fleet.find((entry) => entry.connection.deckId === agentView.deckId) : undefined;
  /**
   * PRD #1105 — whether the open pane's agent can have a terminal at all, which
   * is the one condition two different things read.
   *
   * It decides whether an attach may be declared for that agent, and it decides
   * whether the pane shows a terminal or says why it has none. Those must agree
   * — a pane showing a terminal nothing attached to is the black rectangle this
   * feature exists to replace, and a pane explaining itself while an attach is
   * live would be explaining a state it is not in. One expression is how they
   * are kept in agreement; two comparisons of the same values is how they would
   * drift.
   *
   * # It used to ask whether the deck was SELECTED, and that is what changed
   *
   * The desktop app's reason to exist over the TUI is that it is a control
   * plane for every deck at once (PRD #802, #742) — so an agent the overview
   * lists and cannot open as a working pane is the feature failing on its own
   * terms. The condition was `agentView.deckId === selectedDeckId` because
   * `terminal::attach` resolved its daemon through the process-global
   * `trusted_daemon()`: declaring an attach while another deck was in force
   * attached *that* deck's agent of the same per-daemon monotonic id, on
   * another machine, under the right name.
   *
   * The attach now names its deck and the crate resolves that deck's own link
   * through `DaemonLinks`, so being selected has stopped being a precondition
   * for having a terminal. What remains a precondition is being REACHABLE: a
   * deck that is disconnected, still waiting to report, or configured with no
   * address has no link to attach over, and a pane there must say so rather
   * than mount a viewport that will receive nothing.
   *
   * An agent on a deck this app is not observing at all falls here too, through
   * `paneDeck` being `undefined` — a pane cannot promise an attach against a
   * deck it cannot name.
   */
  const paneDeckAttachable = paneDeck?.connection.status === "connected";
  /**
   * The agent the open pane is FOR, resolved by the composite identity against
   * the fleet entry named by the view — never by bare id against the selected
   * deck's snapshot, which is where a bare-id lookup would find a *different*
   * agent wearing the same per-daemon monotonic id (PRD #1105 M6).
   *
   * Resolved HERE rather than inside {@link OverviewAgentPane}, and for the same
   * reason `paneDeckAttachable` is one expression: three things read it — the
   * shown declaration below, the retirement close further down, and the pane
   * itself, which takes it as a prop. Two copies of this `find` is how those
   * would drift, and the drift the PR review found was exactly that shape: the
   * pane resolved the agent and rendered nothing when it failed, while the
   * declaration never asked and went on naming it.
   */
  const paneAgent = agentView && paneDeck ? paneDeck.agents.find((candidate) => candidate.id === agentView.agentId) : undefined;
  /**
   * Issue #1143 — the last record this pane's deck gave, so the pane can still
   * be a pane while that deck is not answering.
   *
   * **This is what makes PRD #1105's no-terminal state REACHABLE.** The PRD
   * decided that such a deck *"replaces its terminal with a sentence rather than
   * closing the pane"*, and only the second half was true in production:
   * `paneAgentRetired` below kept the view, but everything the pane draws is a
   * property of the agent RECORD, and a deck with no live link reports no
   * agents at all — so `paneAgent` was `undefined`, the render condition failed,
   * and the state built for exactly this case never rendered.
   *
   * It is read ONLY where the live lookup found nothing, so a deck that is
   * answering is never served an older record than the one it just sent. Which
   * is also why the two conditions under it are left reading `paneAgent`
   * directly rather than this: `paneAgentRetired` must close on a CONNECTED
   * deck that has stopped listing the agent, and a held record would make that
   * absence invisible; the shown declaration must name an agent that exists
   * now, and a held record is by definition one nothing can be attached to.
   *
   * **The un-observed deck stays as it was, deliberately.** `paneDeck` being
   * `undefined` — a deck that has left the observed set entirely — still
   * renders no pane, because the state needs the deck's own account of why it
   * is not answering and a deck the app is no longer watching gives none.
   * Synthesising one would be this app inventing a failure it did not observe,
   * which is the distinction `pruneFleet` already *"refuses to cross on its
   * own"* (`lib/bridge.ts`). That case keeps today's behaviour: the view
   * survives and the pane returns if the deck does.
   *
   * **The DECK-origin pane is not held either, and that is measured rather than
   * assumed.** A promoted pane marks everything off its own ancestor path
   * `inert` ({@link useInertBackground}, whose doc names a connection banner as
   * exactly the background content it re-marks), so a held deck-origin pane
   * would explain that the deck is not answering while putting the `Reconnect`
   * control that fixes it behind an inert barrier — verified on the fixture:
   * banner `inert`, button inheriting it. The deck screen already says the
   * thing, in its own banner, with the remedy attached, and shows no tiles a
   * reader could misread as live; the overview has no such sentence anywhere,
   * which is what makes the pane the only surface that can speak there. Both
   * halves are pinned by `leaves a deck-origin pane's screen to explain itself,
   * with its remedy reachable`.
   */
  const heldPaneAgent = useHeldAgentRecord(agentView, paneAgent);
  /** The record the pane RENDERS: the deck's current answer, else its last one. */
  const paneAgentShown = paneAgent ?? heldPaneAgent?.agent;
  /**
   * PRD #1105 M4 — the shown set for the OVERVIEW tree, declared here because
   * this is the only component that can see the overview and the pane over it
   * in one commit. `undefined` on the deck path hands ownership to
   * {@link DeckSurface} without declaring anything; see {@link useShownTerminals}.
   *
   * The declaration names the pane's OWN deck, never the selected one, so a
   * selection move neither retargets it nor tears it down: the joined key is
   * unchanged, so no call is made at all.
   *
   * **It names an agent that still RESOLVES, which is the PR review's P1.** The
   * declaration used to gate on the deck alone, so an agent the daemon ended
   * under an open pane stayed declared — and the standing declaration is not
   * inert. `useShownTerminals` keys its effect on the joined `(deckId,
   * agentId)` string, which an agent leaving the fleet does not change, so
   * nothing re-fired to withdraw it; meanwhile the bridge re-runs
   * `attachAgents` over the whole shown set on **every** `desktop://snapshot`
   * (`bridge.ts`'s `subscribe`), and the daemon's `end` event for that agent has
   * already removed it from `attached` — so it cleared `attachAgents`' filter
   * afresh on each snapshot, took the process-wide attach gate and opened a
   * socket for an agent that no longer exists, serialising against the attaches
   * of panes that do.
   */
  const overviewShown = base === "overview"
    ? (agentView && paneAgent && paneDeckAttachable ? [{ deckId: agentView.deckId, agentId: agentView.agentId }] : [])
    : undefined;
  useShownTerminals(runtime.setShownTerminals, overviewShown);
  /**
   * PRD #1105's security audit — the deck-origin pane is FENCED on identity,
   * and closes rather than retargets.
   *
   * A deck-origin pane means "this agent, on the deck you are looking at": the
   * deck surface renders the selected deck and no other, so the pane's claim
   * and the view's `deckId` are the same claim. When the selected deck moves
   * out from under it — by the deck selector under an open pane, by a
   * `selectionFallback` the crate reports, or by another window writing the
   * settings document — that claim is no longer true of anything on screen.
   * Holding the pane open would promote the NEW deck's agent of the same
   * per-daemon monotonic id under the old one's name, with matching role text
   * and no deck identity in the dialog, and would route the user's keystrokes
   * to it.
   *
   * Closing is chosen over suspending because it is the only option that
   * leaves nothing to be wrong about: a suspended pane still has to say whose
   * agent it is showing, and the honest answer is "nobody's". The user lands on
   * the deck they just selected, which is what they asked for.
   *
   * The render below does not wait for this effect. `DeckSurface` promotes a
   * tile only on the full `(deckId, agentId)` match, so the mismatch commit
   * shows no pane at all and this closes the view behind it — the pane's
   * lookup, its shown declaration and its input authority never degrade to a
   * bare id, not even for one frame.
   *
   * An UNKNOWN selected deck is not a mismatch, which is why this is its own
   * comparison rather than `!paneDeckSelected` above. `reconnect`'s failure
   * path rebuilds `connection` without a `deckId`, and a transiently
   * unidentified deck is not evidence that the pane's deck changed — where for
   * the attach declaration it is exactly grounds to declare nothing. The two
   * conditions genuinely differ on that one input, and collapsing them would
   * close a deck-origin pane every time a reconnect failed.
   */
  const deckPaneRetargeted = agentView?.from === "deck" && selectedDeckId !== undefined && agentView.deckId !== selectedDeckId;
  useEffect(() => {
    if (deckPaneRetargeted) closeAgent();
  }, [deckPaneRetargeted, closeAgent]);
  /**
   * PRD #1105 open question 3, answered — **the view does not outlive its
   * subject.** The daemon can end an agent while it is overlaid, and before this
   * the app simply stayed in `view.kind === "agent"`: the pane's own lookup
   * failed, it returned `null`, and what was left was a view with nothing
   * rendering it, no Close control, and (on the overview path) a shown
   * declaration still naming the agent.
   *
   * Closing is chosen over an explicit ended state for three reasons, the first
   * of which is decisive. **Everything the pane renders is a property of the
   * agent RECORD** — heading, role, status, prompt, tool, the five panel tabs —
   * and that record is what has gone; an ended state would have to be built
   * from the view, which carries two ids and a `from`. Second, it is the answer
   * already given one condition above for the same shape of problem:
   * `deckPaneRetargeted` closes rather than suspends because *"a suspended pane
   * still has to say whose agent it is showing, and the honest answer is
   * 'nobody's'"*. Third, `closeAgent` lands on `view.from` — for an
   * overview-origin pane, the screen that lists what does exist, which is where
   * the absence explains itself. The PRD records the *visual* treatment as
   * undecided, and closing is the option that decides nothing on the owner's
   * behalf: it adds no surface, and an ended state can still replace it later.
   *
   * # `paneDeckAttachable` is load-bearing, and closing without it is worse than
   * the defect
   *
   * A deck that is not answering reports **no agents at all** — both empty
   * agent lists in the crate are on that path, `disconnected_snapshot` and
   * `snapshot_with`'s non-connected early return — and `mapDesktopSnapshot`
   * carries none over from the previous snapshot. So "the agent does not
   * resolve" is ALSO what a remote deck blinking, a failed reconnect, or a deck
   * that has not yet reported looks like, and closing on that alone would throw
   * a healthy pane away on each of them. It would also contradict this PRD's own
   * decision that such a deck *"replaces its terminal with a sentence rather
   * than closing the pane"*.
   *
   * A CONNECTED deck's list is an answer rather than a silence:
   * `connected_snapshot` is the only producer of a connected snapshot and it
   * maps a `ListAgents` reply every time — fresh, or the agent view's cached
   * records — and a `ListAgents` that fails becomes a `disconnected_snapshot`
   * instead. So an absence there is a real absence, and that is the whole of
   * why this reads `paneDeckAttachable`, the expression that already means
   * "this deck has a live link", rather than testing `paneAgent` alone.
   *
   * # Both origins, one condition
   *
   * The deck path's shown declaration was never at risk — {@link DeckSurface}
   * derives it from `snapshot.agents`, so a retired agent shrinks the set and
   * the joined key changes on its own — and its render self-heals, because
   * `paneAgentId` resolves to `undefined` and promotes no tile. What it shared
   * with the overview was precisely this stale view, so the condition is written
   * for `agentView` rather than for `base === "overview"`.
   */
  const paneAgentRetired = agentView !== undefined && paneDeckAttachable && paneAgent === undefined;
  useEffect(() => {
    if (paneAgentRetired) closeAgent();
  }, [paneAgentRetired, closeAgent]);
  /**
   * PRD #802 M6 — run one resolved voice command, and answer with how to undo it.
   *
   * # It dispatches through the registry, and that is the design's central rule
   *
   * `dispatchVoiceAction` looks the outcome's `invoke` up in `VOICE_ACTIONS` and
   * calls the same `run` the rail button and the palette item call, so the
   * pipeline's step 5 — *hand the resolved action to the existing handler* — is
   * literally true. Voice gets no execution path of its own, and there is no
   * `switch` over action ids here: a switch would be a second list of the ids and
   * would be *implementation* a new command has to edit — which is what PRD
   * #802's criterion forbids. Adding a command costs a row in `commands.toml` and
   * the classification flip in `voiceActions.ts`; the rest of what M8 measured is
   * test assertions pinning the shipped row set by value.
   *
   * # The context is the deck's members plus the shell's two, and a dispatch the
   * host cannot serve is REFUSED rather than attempted
   *
   * `navigate` and `closeAgentView` are the two the SHELL owns, and they are
   * always there. The overlay togglers, the tile selection and the fixture
   * stepper belong to `DeckSurface`, which publishes them up through
   * `deckVoiceContext` while it is mounted (PRD #802 M7) — so a row naming one of
   * them is servable on the deck and the overlay rows stopped being a plumbing
   * project.
   *
   * **The shell's two are spread LAST on purpose.** The deck's own `navigate`
   * and `closeAgentView` forward to the props this component passed it, so they
   * are the same navigations by a longer route; taking the shell's directly
   * keeps behaviour identical whether or not a deck happens to be mounted, which
   * is what stops the overview and the deck disagreeing about what a command
   * does.
   *
   * On the overview the slot reads `undefined` and the context is the two alone.
   * That is not a silent narrowing: `dispatchVoiceAction` checks each entry's
   * declared `needs` first and answers `false`, which becomes `undefined` here
   * and `NOTHING_DISPATCHED` in the report. The alternative was what M6 shipped —
   * `TypeError: context.openOverlay is not a function`, rendered at the user.
   *
   * # The undo is a view, captured before the command runs — and OFFERED only
   * where one moved
   *
   * *Undo* here means "put the screen back". The destination is read off `view`
   * at dispatch time rather than popped from a stack, which is the same choice
   * `closeAgent` makes one screen up, and for the same reason: there is no
   * history to be wrong about.
   *
   * **Not every command is a navigation any more.** `open_settings` puts an
   * overlay over the deck and leaves the view exactly where it was, so a
   * `setView(previous)` for it restores a screen nothing left and the button
   * would sit there doing visibly nothing — an affordance lying about what it
   * reverses. So the navigation is OBSERVED rather than declared: the two
   * view-moving members are wrapped here, and the Undo is offered only if one of
   * them was actually called. That needs no new list beside the registry, and it
   * uses a distinction `VoiceControlPanel.onDispatch` already draws — an object
   * with no `undo` means *it ran and there is nothing to reverse*, which is not
   * the same answer as `undefined`.
   *
   * What an overlay's real undo would be — closing that overlay — is a reverse
   * for each entry rather than one for the shell, and nothing in this slice
   * needs it.
   *
   * **The `aimed` flag that used to sit beside `moved` is gone with the
   * dictation MODE it was about.** Aiming the microphone at an agent moved the
   * screen as well, so an Undo beside it would have restored the previous
   * screen while voice stayed pointed at the agent — an affordance lying about
   * what it reverses. Dictation no longer moves anything or leaves anything
   * aimed: it types one utterance into the pane that is already open, so there
   * is no navigation to observe and nothing for the flag to suppress.
   */
  const dispatchVoice = useCallback((outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }>, declaredDirectories?: VoiceDirectoriesDto, declaredNewAgent?: VoiceNewAgentDto) => {
    const previous = view;
    /*
      One target for every entry, built from the outcome's own resolved params —
      an entry reads the members it declared and ignores the rest. The agent id
      is the `value` Rust resolved against live state, never the `spoken` word.

      `deckId` falls back to the empty string only where the selected deck has no
      identity, which is a state no dispatch can reach: an unidentified deck is a
      deck that answered no `ListAgents`, so it reported no agents, so an
      `agent_ref` param resolved to nothing and the outcome was `param_unresolved`
      rather than this.
    */
    const agent = outcome.params.find((param) => param.kind === "agent_ref");
    /* PRD #802 D6, rebuilt — the words to TYPE, resolved Rust-side from the
       transcript. `value` rather than `spoken`: `spoken` is the boundary the
       model marked, and `value` is what this app resolved that boundary to in
       its own transcript. The model's string never reaches a terminal. */
    const dictated = outcome.params.find((param) => param.kind === "spoken_prefix");
    /* PRD #1223 — the deck a `deck_ref` resolved to, against the observed
       fleet, Rust-side. Its own member rather than `deckId`, which falls back
       to the selected deck below and so cannot say "the user named none". */
    const namedDeck = outcome.params.find((param) => param.kind === "deck_ref");
    /* PRD #1223 — the child a `dir_ref` resolved to, against the browser's
       children on screen: its `value` is the deck's own path for it. */
    const namedDirectory = outcome.params.find((param) => param.kind === "dir_ref");
    /* PRD #1223 — the Mode chip and the agent entry a `mode_ref` and an
       `agent_type_ref` resolved to, against the form AS DECLARED: `value` is
       the id the dialog selects by. */
    const namedMode = outcome.params.find((param) => param.kind === "mode_ref");
    const namedAgentType = outcome.params.find((param) => param.kind === "agent_type_ref");
    const declaredForm = declaredNewAgent?.form;
    /* PRD #1223 — the orchestration card an `orchestration_ref` resolved to,
       named by one member's agent id on the selected deck, whose agents Rust
       resolved it against. */
    const namedOrchestration = outcome.params.find((param) => param.kind === "orchestration_ref");
    const target: VoiceDispatchTarget = {
      /* The dictation pair targets the pane on SCREEN — its row declares no
         agent param and is `screens = ["agent"]`, so `agentView` is defined
         whenever one of them dispatches. The `agent_ref` value still wins where
         a row resolved one, which is every row that takes an agent. */
      deckId: agent ? (selectedDeckId ?? "") : (agentView?.deckId ?? selectedDeckId ?? ""),
      agentId: agent?.value ?? agentView?.agentId ?? "",
      from: base === "overview" ? "overview" : "deck",
      /* What the DECK calls it, which is what Rust resolved the spoken words
         against — never the spoken words themselves. A surface that named the
         agent the way the user said it would confirm the mishearing rather
         than expose it. */
      /* With no `agent_ref` to resolve, the pane's OWN agent supplies it — the
         countdown line has to name the agent the way the screen does, and an
         id where a display name belongs is exactly where a user would fail to
         notice they were typing into the wrong one. */
      agentLabel: agent?.label ?? paneAgent?.displayName,
      text: dictated?.value,
      agentViewOpen: agentView !== undefined,
      ...(namedDeck ? { preselectDeckId: namedDeck.value } : {}),
      ...(namedDirectory ? { directoryPath: namedDirectory.value } : {}),
      /* What the utterance was judged against, so a directory move can refuse
         a browser that has moved on since (see the member's own comment). */
      ...(declaredDirectories ? { declaredDirectories: { deckId: declaredDirectories.deckId, path: declaredDirectories.path } } : {}),
      ...(namedMode ? { modeId: namedMode.value } : {}),
      ...(namedAgentType ? { agentTypeId: namedAgentType.value } : {}),
      ...(declaredForm ? { declaredForm: { deckId: declaredForm.deckId, path: declaredForm.path } } : {}),
      ...(namedOrchestration ? { orchestrationAgentId: namedOrchestration.value } : {}),
    };
    let moved = false;
    const context: VoiceDispatchContext = {
      ...deckVoiceContext.current,
      /* The overview's, which is never mounted beside the deck, so the two
         cannot both publish (PRD #1223 U5). */
      ...overviewVoiceContext.current,
      /* After the deck's, and the two sets are disjoint by construction — see
         `VoicePanelContext`, which is a narrow `Pick` precisely so a screen and
         the voice surface can never offer the same member. */
      ...panelVoiceContext.current,
      navigate: (next) => { moved = true; setView(next); },
      closeAgentView: () => { moved = true; closeAgent(); },
    };
    if (!dispatchVoiceAction(outcome.invoke, context, target)) return undefined;
    return moved ? { undo: () => setView(previous) } : {};
  }, [agentView, base, closeAgent, paneAgent, selectedDeckId, view]);
  /** PRD #1223 — what the directory browser shows, read at declaration time. */
  const readDirectories = useCallback(() => newAgentVoice.current?.directories, []);
  /** PRD #1223 — what the New agent dialog shows besides its browser, while it is open. */
  const readNewAgent = useCallback(() => newAgentVoice.current?.newAgent, []);
  /* Which mount of the dialog that declaration came from — never sent to Rust,
     read only to refuse an answer whose dialog has been replaced (PRD #1223). */
  const readNewAgentInstance = useCallback(() => newAgentVoice.current?.instance, []);
  /* The COMPOSITE identity, never the bare id. See `deckPaneRetargeted` above
     and `DeckSurface`'s own promotion condition. */
  const openAgent = agentView ? { deckId: agentView.deckId, agentId: agentView.agentId } : undefined;
  const screenNode = base === "overview"
    ? (
      <>
        <AgentOverview runtime={runtime} settings={settings} onNavigate={setView} agentPaneOpen={agentView !== undefined} voiceChannel={overviewVoiceContext} newAgentVoice={newAgentVoice} />
        {/*
          The overview mounts no terminal of its own (PRD #745's commitment), so
          there is no tile here to promote and the pane is a sibling of the
          screen rather than a promotion inside it. That still leaves exactly
          one live `TerminalViewport` for the agent, which is the property M3
          actually requires.
        */}
        {agentView && paneDeck && paneAgentShown && <OverviewAgentPane runtime={runtime} view={agentView} deck={paneDeck} agent={paneAgentShown} held={heldPaneAgent} attached={paneDeckAttachable} onClose={closeAgentView} />}
        {/*
          PRD #1223 audit W2 — the runtime's last failure, on THIS screen too.

          The New agent flow lives here, and its dialog deliberately leaves the
          runtime's error alone once it is unmounted (`NewAgentDialog`'s
          `mounted` ref) because by then it is the only copy of the failure.
          The only surface rendering that copy was the deck's toast, which is
          unmounted whenever this screen is up — so a launch that failed with
          roles still possibly running was reported nowhere, and this screen's
          Refresh calls `reconnect()`, which clears it unseen.

          `runtime.error` alone: the notice beside it on the deck is that
          screen's own state, and this screen has none. The two are never
          mounted together, so there is at most one toast.
        */}
        {runtime.error && <Toast message={runtime.error} cleanup={runtime.errorCleanup} onDismiss={runtime.clearError} />}
      </>
    )
    : <DeckSurface runtime={runtime} settings={settings} workflowPlatformIssue={workflowPlatformIssue} onNavigate={setView} openAgent={openAgent} onCloseAgent={closeAgent} voiceChannel={deckVoiceContext} />;
  /*
    PRD #802 M6 — the voice surface is a SIBLING of the screen switch, and this
    shape is the whole of that decision.

    The two screens replace one another: the deck is unmounted while the overview
    is up, which is how the zoom keys came to be dead on the overview (see
    `settings` above). A voice report describes a navigation that has just
    happened and the Undo beside it reverses one, so a panel mounted inside
    either screen would be torn down by the very command it was reporting — the
    sentence and the Undo would go with it, and `open_overview` would be the one
    command whose report nobody ever sees.

    The two children are POSITIONAL, which is what makes the panel survive: React
    reconciles by position, so the screen at index 0 may change type freely while
    index 1 keeps the same fiber and therefore the same panel state. This
    deliberately returns one fragment on both paths rather than the screen alone
    on one of them, because a fragment on one path and a `DeckSurface` on the
    other is a different root type and would remount everything under it.
  */
  return (
    <>
      {screenNode}
      <VoiceControlPanel runtime={runtime} screen={view.kind} onDispatch={dispatchVoice} channel={panelVoiceContext} directories={readDirectories} newAgent={readNewAgent} newAgentInstance={readNewAgentInstance} />
    </>
  );
}

/**
 * The pane over the OVERVIEW, with the tile state the deck would otherwise
 * have owned. Its terminal is live whichever deck the agent is on.
 *
 * Split out for the hook and nothing else: the panel tab is component state,
 * and a `tabs` map kept in {@link DeckShell} for a screen that has no tiles is
 * the alternative.
 *
 * # It takes its deck and its agent, and resolves neither
 *
 * Both are resolved by {@link DeckShell} — `paneDeck` and `paneAgent` — and
 * handed down, so this component has no lookup that can fail and no `null`
 * branch. That is the PR review's P1 fixed at the structure rather than at the
 * symptom: the early return this used to open with was a **silent** answer to
 * "the agent is gone", and the parent that owns the view and the shown
 * declaration never learnt of it. The parent now decides — closing the view
 * when the pane's deck is answering and does not list the agent, and holding it
 * when the deck is simply not answering — and this pane is rendered only where
 * there is something to render.
 *
 * The identity behind that lookup is the COMPOSITE one (PRD #1105 M6): this
 * screen merges every observed deck and every agent it lists is openable, so
 * the pane can be for an agent on a deck that is not the selected one — and the
 * selected deck's snapshot is exactly where a bare-id lookup would find a
 * *different* agent wearing the same per-daemon monotonic id. `Escape` closes
 * the view from here as from anywhere, because that listener is
 * {@link DeckShell}'s and not this component's.
 *
 * # `attached` is the pane's whole deck story, and it is a state rather than a
 * refusal
 *
 * The pane attaches on its OWN deck — {@link DeckShell}'s `paneDeckAttachable`
 * is the one condition that decides both the shown declaration and this, so the
 * two cannot disagree. It is false only where that deck has no live link at
 * all: disconnected, not yet reporting, or not observed. The pane opens either
 * way — everything that is a property of the AGENT works, header, status,
 * prompt, tool, all five panel tabs, `Esc` and the close control — and the
 * terminal tab then renders an explicit no-terminal state naming the deck and
 * what is wrong with it instead of a `TerminalViewport` that would receive no
 * bytes.
 *
 * **"Opens either way" needed a record to open WITH, and issue #1143 is where
 * that came from.** A deck with no live link reports **no agents** —
 * `disconnected_snapshot` and `snapshot_with`'s non-connected early return both
 * carry `agents: Vec::new()`, and `mapDesktopSnapshot` carries none over from
 * the previous snapshot — so until #1143 the pane this state was built for was
 * not rendered at all on a deck that stopped answering, and only the view was
 * guaranteed to survive (see `paneAgentRetired`). {@link DeckShell} now holds
 * the last record the pane's deck gave, for that pane's identity and nothing
 * else, and hands it down through `held`.
 *
 * **A held record is rendered as a past report rather than a present one**, and
 * that is the product call rather than a detail of it: showing a stale header
 * with nothing saying it is stale asserts a liveness this app has no evidence
 * for, which is worse than showing nothing. Three things say it — the status
 * reads `last seen: running` and loses its live colour, `data-agent-record` says
 * `held` for anything reading the DOM, and the no-terminal sentence names how
 * old the report is with the exact instant on its hover. There is no expiry:
 * see {@link useHeldAgentRecord} for why a timestamp was chosen over a timeout.
 *
 * **Nothing here moves the selection.** Switching the selected deck on open was
 * built and withdrawn under this PRD (decision 5) because it wrote
 * `desktop.toml` on a navigation and left state created under one deck
 * attributed to another. The pane reaching its own deck is what made that
 * unnecessary rather than merely unwise.
 */
function OverviewAgentPane({ runtime, view, deck, agent, held, attached, onClose }: { runtime: DeckRuntimeState; view: Extract<DeckView, { kind: "agent" }>; deck: DeckSnapshot; agent: AgentSession; held?: HeldAgentRecord; attached: boolean; onClose: () => void }) {
  const [tab, setTab] = useState<PanelTab>("terminal");
  return (
    <AgentPaneFrame
      open
      agent={agent}
      mode={runtime.mode}
      selected
      tab={tab}
      terminalFeed={runtime.terminalFeed}
      /* The deck named by the fleet entry this pane resolved through — so the
         sentence names the agent's OWN deck, not whichever one is selected, and
         it names it with `deckName`, which is what the overview's group header
         the user just came from calls it. Reached only where that deck has no
         live link: a merely NON-SELECTED deck attaches like any other. */
      /* Issue #1143 — the relative age is computed at RENDER and needs no clock
         of its own, unlike the overview's two relative columns: an unreachable
         deck's watcher takes `spawn_deck_watcher`'s no-link arm every
         iteration, emitting a snapshot before it sleeps `WATCH_RETRY_DELAY`
         (1s), `emit_snapshot` is a bare `app.emit` with no dedupe, and
         `adoptFleet` rebuilds the fleet on each one — so this re-renders an
         order of magnitude more often than `OVERVIEW_CLOCK_TICK_MS` would tick
         it. The `title` needs none of that argument: an absolute instant
         cannot decay. */
      noTerminal={attached ? undefined : unreachableDeckTerminalState(deckName(deck.connection), deck.connection.message, displayActivity(held?.confirmedAt))}
      /* Issue #1143 — `held` is set only where the live lookup found nothing,
         so its presence IS the claim that what the header shows is older than
         now. One prop, read by the two colour cues and the wording together. */
      recordFreshness={held ? "held" : "live"}
      /* This deck's, for the same reason the agent above is: the bridge records
         hook events for the selected deck alone, so a non-selected deck's entry
         carries none rather than somebody else's. The terminal crosses decks;
         the evidence ring does not, and PRD #742 DECISION 1 keeps it that
         way. */
      evidence={deck.evidence}
      /* By the COMPOSITE key, for the same reason the agent above is resolved
         that way: a verdict recorded against the previously selected deck's
         `planner` would otherwise disable this pane and print that deck's
         rejection notice under this agent's heading, and that deck's cached
         grid would be submitted to this agent's PTY — reflowing it, and every
         other viewer attached to it, from a viewport on another machine. */
      inputResult={runtime.terminalInputResults?.[agentKey(view.deckId, agent.id)]}
      onSelect={() => undefined}
      onTabChange={setTab}
      onTerminalInput={runtime.sendTerminalInput}
      onTerminalResize={runtime.resizeTerminal}
      appliedGeometry={runtime.appliedGeometry?.[agentKey(view.deckId, agent.id)]}
      /* The evidence drawer is the deck's, and no screen is mounted here that
         could open it — so the handoffs tab lists evidence and selecting one
         does nothing, rather than pretending at a drawer that is not there.
         `onRename` is absent for the sharper version of the same reason: a
         rename reports its outcome through the deck's toast, and a rename that
         fails silently is worse than a header without the pencil. Both are
         capabilities this screen genuinely lacks rather than presentation
         differences, which is why neither is expressed through
         `presentation`. */
      onEvidenceSelect={() => undefined}
      onClose={onClose}
    />
  );
}

/**
 * PRD #1105 M3 — the pane's WRAPPER, and the reason it is rendered whether or
 * not the pane is open.
 *
 * It positions {@link AgentTile} and carries the dialog chrome AROUND it; it
 * renders no part of the pane itself. That line is what keeps the PRD's "no
 * `AgentTileLarge`" criterion checkable — everything inside the box is one
 * component at two presentations, and everything outside it is position and
 * role.
 *
 * **Always rendered, because promote-in-place is a property of the React
 * tree.** React reconciles children by position, key and TYPE, so this element
 * has to exist at the tile's position in both states: flipping `open` then
 * changes this `div`'s attributes and the tile's `presentation`, and React
 * keeps the same `AgentTile` fiber, the same `TerminalViewport` beneath it and
 * therefore the same xterm instance with its scrollback, selection and cursor.
 * Rendering the wrapper only when open would swap a `div` in where an
 * `AgentTile` was, which is an unmount — a rebuilt xterm and a client-side
 * transcript re-write on every open and every close.
 *
 * Closed, the wrapper is `display: contents`, so `.agent-tile` remains the
 * grid item it has always been and the deck's layout is untouched.
 */
function AgentPaneFrame({ open, onOpen, onClose, ...tile }: Omit<AgentTileProps, "presentation"> & { open: boolean }) {
  /*
    The `aria-modal` below is a claim about the whole interface, and until the
    PRD's security audit it was false: the base screen is deliberately still
    mounted underneath, so every control on it — the rail, the other tiles, and
    the `DeckSelector` that can retarget the app to another deck — stayed
    keyboard-reachable behind a full-window dialog. `useInertBackground` is what
    makes the attribute true, by marking every element that is not an ancestor
    of this one inert and moving focus inside. See that hook for why it walks
    siblings rather than marking one subtree.
  */
  const paneRef = useInertBackground<HTMLDivElement>(open);
  return (
    <div
      ref={paneRef}
      className={open ? "agent-pane-overlay" : "agent-pane-slot"}
      data-testid={open ? "agent-pane-overlay" : undefined}
      role={open ? "dialog" : undefined}
      aria-modal={open ? "true" : undefined}
      aria-label={open ? `${tile.agent.role} agent` : undefined}
      /* Focusable only as a focus TARGET, never as a tab stop: the hook moves
         focus here when the pane opens over a control that had it, and Tab then
         proceeds into the pane's own controls. */
      tabIndex={open ? -1 : undefined}
    >
      <AgentTile
        {...tile}
        presentation={open ? "overlay" : "tile"}
        /* One control at a time, and by construction: the pane that is open
           offers Close and the tiles behind it offer Open. */
        onOpen={open ? undefined : onOpen}
        onClose={open ? onClose : undefined}
      />
    </div>
  );
}

/**
 * The deck with a settings state of its own.
 *
 * A three-line wrapper so the deck can still be rendered standalone — which is
 * what the tests do, and what every caller did before the settings state moved
 * up to {@link DeckShell}. The alternative was an optional `settings` prop on
 * {@link DeckSurface} falling back to its own hook, and that carries a trap
 * this does not: two live settings instances, one of them rendered by nothing,
 * so a later `save` against the wrong one would write state no screen reads.
 * Here there is exactly one instance per tree.
 */
export function ControlDeck(props: { runtime: DeckRuntimeState; workflowPlatformIssue?: string; onNavigate?: (view: DeckView) => void }) {
  const settings = useDesktopSettings(props.runtime);
  return <DeckSurface {...props} settings={settings} />;
}

export function DeckSurface({ runtime, settings, workflowPlatformIssue = desktopWorkflowPlatformIssue(), onNavigate, openAgent, onCloseAgent, voiceChannel }: { runtime: DeckRuntimeState; settings: DesktopSettingsState; workflowPlatformIssue?: string; onNavigate?: (view: DeckView) => void; openAgent?: { deckId: string; agentId: string }; onCloseAgent?: () => void; voiceChannel?: VoiceContextChannel }) {
  const { snapshot, mode, setShownTerminals } = runtime;
  /**
   * Which tile is promoted, decided on the FULL `(deckId, agentId)` identity.
   *
   * PRD #1105's security audit. This prop was a bare `openAgentId`, and agent
   * ids are per-daemon monotonic — so when the selected deck moved under an
   * open pane, the arriving deck's namesake matched and was promoted into it.
   * Same role, same display text, no deck identity in the dialog, and
   * `sendTerminalInput(agentId, …)` resolving the bare-id session that now
   * belonged to the other machine. `DeckShell` closes the view when that
   * happens; this is the render-synchronous half, so no such commit exists even
   * before the effect runs.
   *
   * Every agent here carries the selected deck's `daemonId`, so this is exactly
   * the comparison `DeckShell` makes — expressed per tile, at the seam that
   * decides what the user sees, rather than trusted from the caller.
   */
  const paneAgentId = openAgent && snapshot.agents.some((agent) => agent.id === openAgent.agentId && agent.daemonId === openAgent.deckId)
    ? openAgent.agentId
    : undefined;
  const [selectedAgentId, setSelectedAgentId] = useState("");
  const [tabs, setTabs] = useState<Record<string, PanelTab>>({});
  const [selectedEvidenceId, setSelectedEvidenceId] = useState("");
  const [evidenceOpen, setEvidenceOpen] = useState(evidenceOpenOnFirstLoad);
  const [projectsOpen, setProjectsOpen] = useState(false);
  const [profilesOpen, setProfilesOpen] = useState(false);
  const [promptsOpen, setPromptsOpen] = useState(false);
  const [selectedPromptId, setSelectedPromptId] = useState("");
  const [terminalFocus, setTerminalFocus] = useState<{ agentId: string; token: number }>();
  const [workflowOpen, setWorkflowOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [noticeState, setNoticeState] = useState<string>();
  /**
   * PRD #1223 audit V7 — the roles a failed launch could not confirm are
   * stopped, shown above whichever message the toast is carrying. Held beside
   * the notice rather than folded into it so a later notice cannot inherit an
   * older failure's roles: {@link setNotice} replaces both at once.
   */
  const [noticeCleanup, setNoticeCleanup] = useState<readonly string[]>();
  const notice = noticeState;
  const setNotice = useCallback((message?: string, cleanup?: readonly string[]) => {
    setNoticeState(message);
    setNoticeCleanup(message === undefined ? undefined : cleanup);
  }, []);
  const [confirm, setConfirm] = useState<ConfirmState>();
  // Memoised so the context value is stable across renders; `runtime.testEndpoint`
  // is itself stable for the lifetime of the bridge.
  const settingsBridge = useMemo(
    () => ({
      testEndpoint: runtime.testEndpoint,
      secretStatus: runtime.secretStatus,
      storeSecret: runtime.storeSecret,
      forgetSecret: runtime.forgetSecret,
    }),
    [runtime.testEndpoint, runtime.secretStatus, runtime.storeSecret, runtime.forgetSecret],
  );
  const { profiles, updateProfile, resetProfiles } = useAgentProfiles(snapshot.profiles);
  /*
   * PRD #819 M6: the projects come from the daemon and nothing is remembered.
   * `useProjects` used to seed a `localStorage` list from the desktop's own
   * guess at a working directory and treat it as the source of truth for the
   * launch cwd — which, against a remote daemon, named a directory on the wrong
   * machine and did not error.
   *
   * The listing is re-fetched when the connection changes and when the agent
   * set moves, because enumeration is derived from LIVE state: a project stops
   * being known the moment its last agent exits.
   *
   * PRD #819 Greptile P2(c): the key used to be `status:agentCount`, and a count
   * does not determine the answer. The daemon derives its project list from its
   * startup cwd, every live agent's `cwd`, every orchestration role's
   * `orchestration_cwd` and every registered schedule's working directory — so
   * replacing an agent with one in another project keeps the count identical and
   * changes the list. The key below carries the seeds this client can actually
   * observe, deduplicated and sorted so it is a set rather than an ordering, and
   * keeps the agent count as well: the key is then a strict superset of the old
   * one, and cannot re-list less often than it did.
   *
   * Issue #887 closed the third seed: registered schedule directories. They
   * still reach no desktop surface and none should — the app shows no schedule
   * — so what travels is `scheduleRevision`, one monotonic integer the daemon
   * bumps whenever its registered task set changes. Registering a schedule in a
   * directory the daemon has nothing else running in adds a project, and the
   * key can now move in response instead of leaving the picker a manual
   * **Refresh** away from the truth. Absent from a daemon that does not report
   * one, which reverts exactly to the previous behaviour rather than to a
   * spurious re-list.
   *
   * The last seed stays deliberately absent and is not a gap: the daemon's own
   * startup cwd is fixed for that daemon's life, so a change in it implies a
   * different daemon, which `socketPath` below now carries.
   *
   * **`socketPath` leads, because the rest of the key is only meaningful
   * WITHIN one deck** (issue #887, Greptile P1). Every other component is a
   * fact about a particular daemon's world — and `scheduleRevision` most
   * sharply so, since it counts from 0 on each daemon start and so is
   * comparable only against earlier values from the same connection. Switching
   * between two connected decks that happen to agree on status, agent count,
   * working directories and revision left the key identical, so `useProjects`
   * never re-listed and the picker kept offering the deck the user had just
   * switched AWAY from. `connection.socketPath` is the per-daemon identity the
   * bridge already uses as `daemonId`, and putting it first makes every
   * comparison below it a within-deck one.
   */
  const projectsRevision = useMemo(() => {
    const seeds = snapshot.agents.flatMap((agent) => [agent.cwd, agent.tab.kind === "orchestration" ? agent.tab.cwd : undefined]);
    const distinct = [...new Set(seeds.filter((cwd): cwd is string => Boolean(cwd)))].sort();
    // NUL as the separator: `is_valid_cwd` refuses it, so no directory can spell
    // one and no two different seed sets can collapse onto the same key. The
    // schedule revision is an integer and joins on the same separator; `""` for
    // an unreporting daemon is a value no revision can spell, so it cannot be
    // confused with revision 0.
    return [
      snapshot.connection.socketPath ?? "",
      snapshot.connection.status,
      String(snapshot.agents.length),
      snapshot.scheduleRevision === undefined ? "" : String(snapshot.scheduleRevision),
      ...distinct,
    ].join("\u0000");
  }, [snapshot.agents, snapshot.connection.socketPath, snapshot.connection.status, snapshot.scheduleRevision]);
  const projectState = useDaemonProjects({
    listProjects: runtime.listProjects,
    resolveProject: runtime.resolveProject,
    revision: projectsRevision,
    enabled: mode === "live" && snapshot.connection.status === "connected",
  });
  const activeProject = projectState.selected;
  const { prompts, addPrompt, updatePrompt, removePrompt } = usePromptLibrary();
  const [profileOrder, setProfileOrder] = useState<string[]>([]);

  // PRD #743: applied on LOAD as well as on change. Keeping it in one effect
  // keyed on the stored value means the panel only has to save — the change
  // is optimistic in `useDesktopSettings`, so this runs on the click rather
  // than after the disk write, and there is no restart and no second path
  // that could disagree with this one.
  useEffect(() => {
    // Issue #845: NOT before the mode is one somebody chose. Until then
    // `settings` holds the unread placeholder — mode "system" — and applying
    // System is `removeAttribute`, which would erase the attribute the Rust
    // side put on the root before this bundle ran (`pre_paint_script` in
    // `src-tauri/src/appearance.rs`) and flash the OS palette on the way to a
    // value it already had. `chosen` rather than `loaded`, because a choice
    // made before the read resolves must still apply on the click.
    if (!settings.chosen) return;
    applyAppearance(settings.settings.appearance.mode);
  }, [settings.chosen, settings.settings.appearance.mode]);

  useEffect(() => {
    if (!selectedAgentId || !snapshot.agents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(snapshot.agents[0]?.id ?? "");
    }
  }, [selectedAgentId, snapshot.agents]);

  useEffect(() => {
    if (!selectedEvidenceId || !snapshot.evidence.some((item) => item.id === selectedEvidenceId)) {
      setSelectedEvidenceId(snapshot.evidence[0]?.id ?? "");
    }
  }, [selectedEvidenceId, snapshot.evidence]);

  useEffect(() => {
    if (!selectedPromptId || !prompts.some((prompt) => prompt.id === selectedPromptId)) {
      setSelectedPromptId(prompts[0]?.id ?? "");
    }
  }, [prompts, selectedPromptId]);

  useEffect(() => {
    if (!profiles.length || profileOrder.length) return;
    try {
      const stored = JSON.parse(window.localStorage.getItem(WORKFLOW_STORAGE_KEY) ?? "null") as { order?: string[] } | null;
      setProfileOrder(stored?.order?.length ? stored.order : profiles.map((profile) => profile.id));
    } catch {
      setProfileOrder(profiles.map((profile) => profile.id));
    }
  }, [profileOrder.length, profiles]);

  useEffect(() => {
    if (!profileOrder.length) return;
    try {
      window.localStorage.setItem(WORKFLOW_STORAGE_KEY, JSON.stringify({ order: profileOrder }));
    } catch {
      // Keep the workflow preview usable when storage is unavailable.
    }
  }, [profileOrder]);

  /**
   * Every tile currently rendering a terminal (PRD #745 M7). `AgentTile` mounts
   * a terminal whenever its tab is `"terminal"`, which is also the default, so
   * the derivation has to repeat that `?? "terminal"` fallback exactly.
   */
  const shownTerminals = snapshot.agents
    .filter((agent) => (tabs[agent.id] ?? "terminal") === "terminal")
    /* The agent's OWN deck, which on this screen is always the selected one —
       written as the agent's rather than read off the connection so the
       declaration cannot drift from what the tile actually mounted. */
    .map((agent) => ({ deckId: agent.daemonId, agentId: agent.id }));

  /**
   * ONE call per render commit carrying ALL the shown ids, never one call per
   * tile: `setShownTerminals` is declarative, so nine tiles declaring
   * themselves one at a time would leave eight of the nine in the warm set and
   * evict five of them. Deleting this line does not fail a bridge test — it
   * silently leaves the deck with no attached terminals at all.
   *
   * PRD #1105 M4 moved the mechanism into {@link useShownTerminals} without
   * changing what the deck declares, so the deck path's most useful property
   * survives by construction: opening the agent pane over the grid changes
   * neither `snapshot.agents` nor `tabs`, so the set is unchanged, so **no call
   * is made at all** on open or on close.
   *
   * This is the deck tree's owner. The overview tree's is `DeckShell`, and the
   * two screens are mutually exclusive, so exactly one is ever mounted.
   */
  useShownTerminals(setShownTerminals, shownTerminals);

  const orderedStages = snapshot.stages;
  const selectedAgent = snapshot.agents.find((agent) => agent.id === selectedAgentId);
  const selectedEvidence = snapshot.evidence.find((item) => item.id === selectedEvidenceId);
  /**
   * PRD #741 M7. Stop and Replace act on a process on **this** machine, so they
   * are unavailable for a remote deck — and this gate is rendering a refusal
   * that already exists rather than inventing one: `Endpoint::require_local`
   * makes the operation unreachable by type, and `connection.localOnlyReason`
   * is its own sentence.
   *
   * Over a forwarded socket the consequence of not gating is not a no-op:
   * `run_daemon_stop` resolves its target from the socket's peer credentials,
   * which name the local `ssh` client, so Stop would tear the tunnel down and
   * report that a daemon had stopped gracefully.
   */
  const remoteDeck = snapshot.connection.deckKind === "remote";
  const canControlDaemon = !remoteDeck && (snapshot.connection.status === "connected" || snapshot.connection.daemonDetected === true);

  const coordinator = snapshot.agents.find((agent) => agent.isStartRole);

  const perform = async (action: DeckAction, success?: string) => {
    try {
      await runtime.runAction(action);
      if (success) setNotice(success);
    } catch (cause) {
      setNotice(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const renameAgent = async (agentId: string, displayName: string) => {
    await perform({ type: "rename_agent", agentId, displayName }, `Agent renamed to ${displayName}.`);
  };

  /**
   * Issue #1042: the terminal IS the input path now, so the palette's
   * "Message coordinator…" entry puts the caret where the agent's own CLI
   * grammar lives instead of in a composer that no longer exists. It still
   * sends nothing — it selects the agent, shows its terminal, and asks that
   * terminal to take focus.
   */
  const focusTerminal = (agentId: string) => {
    setSelectedAgentId(agentId);
    setTabs((current) => ({ ...current, [agentId]: "terminal" }));
    setTerminalFocus((current) => ({ agentId, token: (current?.token ?? 0) + 1 }));
  };

  /**
   * PRD #802 M2 — the deck's half of the action registry's context, and the one
   * place the rail buttons, the palette entries and the tile's open/close pair
   * reach their state from.
   *
   * A `Record<DeckOverlay, …>` rather than a switch, so adding an overlay to the
   * union is a type error here rather than a silently unreachable case. Each
   * setter is React's own and therefore stable, which is what makes it safe for
   * the `window` keydown effect below to close over the first render's copy.
   */
  const overlaySetters: Record<DeckOverlay, (open: boolean) => void> = {
    projects: setProjectsOpen,
    prompts: setPromptsOpen,
    profiles: setProfilesOpen,
    workflow: setWorkflowOpen,
    settings: setSettingsOpen,
  };
  const closeOverlays = () => Object.values(overlaySetters).forEach((setOpen) => setOpen(false));
  /**
   * **`onNavigate` and `onCloseAgent` stay optional here rather than in the
   * registry.** Both have been optional props since PRD #1105, so a deck
   * mounted without them renders and its Overview button does nothing — the
   * behaviour this move must not change. Deciding what an absent prop means is
   * the host's job; the registry's job is to be the dispatch seam for the rail,
   * the palette and every voice-reachable capability. Five of its `no_voice`
   * entries also have a second `setState` path in this file — `voiceActions.ts`
   * names them, and the narrower claim is the true one.
   */
  const voiceContext: VoiceScreenContext = {
    navigate: (view) => onNavigate?.(view),
    closeAgentView: () => onCloseAgent?.(),
    openOverlay: (overlay) => overlaySetters[overlay](true),
    closeOverlays,
    toggleEvidence: () => setEvidenceOpen((open) => !open),
    selectAgent: setSelectedAgentId,
    focusTerminal,
    advanceFixture: () => { void perform({ type: "advance_fixture" }); },
  };

  /**
   * PRD #802 M7 — publish that context UP to {@link DeckShell}.
   *
   * The shell is the only component mounted for every screen, so it is where a
   * voice dispatch has to be built; but five of the registry's entries reach
   * state that lives here, in this component's `useState` booleans, and the
   * shell cannot see it. Before this, a row naming one of them type-checked,
   * passed rule 13, and threw at the call — with `context.openOverlay is not a
   * function` rendered at the user above the report sentence.
   *
   * **No dependency array, on purpose**, for `useInertBackground`'s reason:
   * `voiceContext` is rebuilt on every render because it closes over this
   * render's props and setters, so re-publishing on every commit is what keeps
   * the shell dispatching through live closures rather than the first ones.
   * Nothing dispatches between a commit and its effects, so the momentary
   * `undefined` a cleanup leaves is unobservable.
   *
   * The cleanup is the load-bearing half: unmounted — which is the whole time
   * the overview is up — the slot reads `undefined`, the shell offers its own
   * two members alone, and `dispatchVoiceAction` refuses an entry needing more
   * instead of calling into a deck that is not there.
   */
  useEffect(() => {
    if (!voiceChannel) return;
    voiceChannel.current = voiceContext;
    return () => { voiceChannel.current = undefined; };
  });

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
        return;
      }
      if (event.key === "Escape") {
        // Not a registry dispatch, and the distinction is worth keeping: this
        // is a blanket DISMISSAL — palette, shortcut sheet, confirm dialog and
        // every overlay at once — rather than the Runs control, which shows the
        // deck. It shares `closeOverlays` so the five setters are written once.
        setPaletteOpen(false); setHelpOpen(false); closeOverlays(); setConfirm(undefined);
        return;
      }
      // Asked here rather than at the top, because the two branches above do not
      // need it: `event.target` is an `EventTarget`, which the DOM does not
      // guarantee is an element — a keydown dispatched on `window` has no
      // `matches` at all. Narrowed with `instanceof` rather than asserted into
      // an `HTMLElement`, so the type system checks this call and whatever is
      // added beside it (#826).
      const target = event.target;
      if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea")) return;
      if (event.key === "?") { event.preventDefault(); setHelpOpen(true); return; }
      if (/^[1-4]$/.test(event.key)) {
        const agent = snapshot.agents[Number(event.key) - 1];
        // Through the registry, because this is the SAME capability the
        // palette's `Focus <role>` entries dispatch and a second path to one
        // capability is what PRD #802's first risk is about. Safe from the
        // listener's stale `voiceContext` because `selectAgent` is React's own
        // setter — every member this handler reaches has to stay that way.
        if (agent) VOICE_ACTIONS.focusAgent.run(voiceContext, { agentId: agent.id });
      }
      if ((event.key === "j" || event.key === "k") && snapshot.evidence.length) {
        const current = Math.max(0, snapshot.evidence.findIndex((item) => item.id === selectedEvidenceId));
        const delta = event.key === "j" ? 1 : -1;
        const next = (current + delta + snapshot.evidence.length) % snapshot.evidence.length;
        setSelectedEvidenceId(snapshot.evidence[next].id);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectedEvidenceId, snapshot.agents, snapshot.evidence]);

  const moveStage = (id: string, direction: -1 | 1) => {
    setProfileOrder((current) => {
      const all = current.length ? current : profiles.map((profile) => profile.id);
      const index = all.indexOf(id);
      const target = index + direction;
      if (index < 0 || target < 0 || target >= all.length) return all;
      const next = [...all];
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  };

  const requestStop = () => {
    if (!selectedAgent) {
      if (!canControlDaemon) return;
      const liveAgents = snapshot.connection.runningAgentCount;
      setConfirm({
        title: "Stop the local deck?",
        body: liveAgents && liveAgents > 0
          ? `The deck reports ${liveAgents} live agent${liveAgents === 1 ? "" : "s"}. This safe stop will be refused until those agents are stopped individually.`
          : "This stops the deck running on this machine. No live agents are reported, so this only shuts down the control service.",
        label: "Stop deck",
        busyLabel: "Stopping…",
        action: async () => { await perform({ type: "stop_daemon" }, "Local deck stopped."); },
      });
      return;
    }
    setConfirm({
      title: `Stop ${selectedAgent.role}?`,
      body: `This sends a stop request to ${selectedAgent.displayName}. Unsaved terminal work may be interrupted.`,
      label: "Stop agent",
      busyLabel: "Stopping…",
      action: async () => { await perform({ type: "stop_agent", deckId: snapshot.connection.deckId ?? "", agentId: selectedAgent.id }, `${selectedAgent.role} stop requested.`); },
    });
  };

  const requestRestartDaemon = () => {
    if (!snapshot.connection.daemonDetected || snapshot.connection.runningAgentCount !== 0) return;
    setConfirm({
      title: "Replace the incompatible deck?",
      body: "The deck now running reports no live agents. Agent Deck will stop it and start the exact build bundled with this desktop app.",
      label: "Replace deck",
      busyLabel: "Replacing…",
      action: async () => { await perform({ type: "restart_daemon" }, "Matching deck started and reconnected."); },
    });
  };

  /**
   * Issue #801. Offered ONLY when the desktop crate says the mismatch is
   * stamp-only — the wire protocol agreed on both sides and the git-describe
   * stamps did not. A genuine protocol mismatch never sets that flag, so this
   * button never appears for one, and the crate would refuse it anyway: the
   * protocol check runs first and the allowance cannot reach it.
   *
   * Unlike Replace daemon this is offered whatever the live-agent count, which
   * is the entire point — replacement is correctly refused while agents are
   * live, and that left an upgraded app with nine running agents no way in at
   * all.
   */
  const requestConnectAnyway = () => {
    if (!snapshot.connection.buildStampMismatchOnly) return;
    setConfirm({
      title: "Connect to a differently-built deck?",
      body: "The wire protocol matched on both sides, so this deck and this app agree on the shape of everything they exchange. They were built from different commits, and a stamp difference can still mean divergent behaviour behind an identical wire — a field whose meaning changed while its shape did not. Agent Deck will connect and keep the mismatch on screen for the rest of this session; nothing is remembered after you quit the app.",
      label: "Connect anyway",
      busyLabel: "Connecting…",
      action: async () => {
        try {
          await runtime.runAction({ type: "allow_build_mismatch" });
          // The allowance is read by the NEXT handshake, so the reconnect is
          // what actually connects; the crate caches no verdict.
          await runtime.reconnect();
          setNotice("Connected to the differently-built deck. The mismatch stays in the connection banner for this session.");
        } catch (cause) {
          setNotice(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  const requestStartDaemon = () => {
    setConfirm({
      title: "Start the local deck?",
      body: "Agent Deck will start the deck on this machine and reconnect this control room. No agent is launched until you explicitly launch a workflow.",
      label: "Start deck",
      busyLabel: "Starting…",
      action: async () => {
        try {
          await runtime.runAction({ type: "start_daemon" });
          setNotice("Local deck started and control channel reconnected.");
        } catch (cause) {
          setNotice(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  const requestLaunch = (config: WorkflowLaunchConfig) => {
    const generatedCount = config.roles.length - config.customCommandCount;
    const commandCopy = config.customCommandCount > 0
      ? `${generatedCount} commands are generated from their current profile fields; ${config.customCommandCount} explicit custom command override${config.customCommandCount === 1 ? " bypasses" : "s bypass"} those fields. Custom commands may carry arbitrary permissions and are not covered by structured permission claims.`
      : `All ${generatedCount} commands are generated from the current provider, CLI, model, effort, and permission fields.`;
    const accessCopy = config.generatedFullAccessCount > 0
      ? ` Among generated commands, ${config.generatedFullAccessCount} role${config.generatedFullAccessCount === 1 ? " runs" : "s run"} unrestricted — Claude Code with bypassPermissions, or Codex with no sandbox — so ${config.generatedFullAccessCount === 1 ? "it acts" : "they act"} without asking.`
      : "";
    setConfirm({
      title: `Launch ${config.displayName}?`,
      body: `This starts ${config.roles.length} live CLI agents in ${config.displayPath} and sends your task prompt to the coordinator. ${commandCopy}${accessCopy} This launch does not rewrite project TOML.`,
      label: "Launch live loop",
      busyLabel: "Launching…",
      action: async () => {
        try {
          // The display twins come off with the two counts: they exist for the
          // dialog and the notice, and the daemon gets the identities alone
          // (PRD #819 audit fix).
          const { customCommandCount: _customCommandCount, generatedFullAccessCount: _generatedFullAccessCount, displayName: _displayName, displayPath: _displayPath, ...launch } = config;
          await runtime.runAction({ type: "start_workflow", ...launch });
          setWorkflowOpen(false);
          await runtime.reconnect();
          setNotice(`${config.displayName} launched with ${config.roles.length} configured roles.`);
        } catch (cause) {
          /*
           * PRD #1223 audit V2 — FIRST, ahead of every refusal-code translation
           * below. Each of those says "Nothing was started", and a launch whose
           * rollback could not confirm a role stopped can carry one of their
           * codes too: a `stale-preparation` refusal of a later role after an
           * earlier one had started is exactly that composite. The roles arrive
           * as data (`LaunchCleanupError`), so the warning does not depend on
           * where the sentence puts them; the sentence itself stays the
           * runtime's error, which takes the toast's place once this is
           * dismissed.
           */
          if (cause instanceof LaunchCleanupError) {
            setNotice(cause.message, cause.unconfirmedStops);
            return;
          }
          const message = cause instanceof Error ? cause.message : String(cause);
          /*
           * PRD #819 M6, state 2. The daemon re-resolves on launch, and
           * enumeration is derived from live state — so a project can stop
           * being known between the moment it was drawn and the moment Launch
           * is pressed, when its last agent exits. That is an ORDINARY outcome,
           * so it is presented the way the empty state is: the picker reopens
           * saying the project is no longer known, rather than a red failure
           * naming a refusal code.
           */
          if (message.includes(PROJECT_UNRESOLVED_CODE)) {
            projectState.clearSelection();
            void projectState.refresh();
            setWorkflowOpen(false);
            setProjectsOpen(true);
            setNotice("That project is no longer one this deck knows — nothing is running there any more. Choose another, or paste its path again.");
            return;
          }
          /*
           * The other ordinary refusal: the project is fine and its config
           * changed under the picker. The remedy the daemon's own sentence
           * names is "resolve again", so do exactly that rather than leaving
           * the user a dead end, and say what happened.
           */
          if (message.includes(PROJECT_STALE_REVISION_CODE) && activeProject) {
            void projectState.select(activeProject.path);
            setNotice("This project's .dot-agent-deck.toml changed since the workflows were listed, so nothing was started. It has been re-read — check the workflow and launch again.");
            return;
          }
          /*
           * The two token-time refusals. Both mean the daemon started nothing,
           * and both have the same remedy — prepare again — so both re-resolve
           * the project to pick up its current revision and say what happened
           * in the words that are true of each. `stale-preparation` is the one
           * an ordinary second launch in the same project produces, because the
           * coordinator context lives at a path fixed per project and the later
           * preparation is the one that survives.
           */
          if ((message.includes(PROJECT_STALE_PREPARATION_CODE) || message.includes(PROJECT_STALE_TOKEN_CODE)) && activeProject) {
            void projectState.select(activeProject.path);
            setNotice(message.includes(PROJECT_STALE_PREPARATION_CODE)
              ? "This launch's prepared coordinator context no longer matches what the deck approved — another launch in this project replaced it, or the project moved. Nothing was started. The project has been re-read; launch again to prepare a fresh one."
              : "The deck no longer holds this launch's preparation — it expired, or the deck was replaced. Nothing was started. The project has been re-read; launch again to prepare a fresh one.");
            return;
          }
          /*
           * Not retryable, and the only one of these that is not: the daemon's
           * platform cannot give the published context an owner-only guarantee,
           * so it withholds the verb rather than publishing without one. Its own
           * sentence already names what to do instead, so pass it through.
           */
          if (message.includes(PROJECT_UNSUPPORTED_PLATFORM_CODE)) {
            setWorkflowOpen(false);
            setNotice(message);
            return;
          }
          setNotice(message);
        }
      },
    });
  };

  /*
   * Issue #1046. The toast shows `notice ?? runtime.error`, so a click can only
   * honestly dismiss what is on screen — and until this issue it could not even
   * do that, since `runtime.error` had no clear and the X was inoperative for
   * every error-sourced message.
   *
   * The error goes with the notice when it IS the notice: a handler that reports
   * a failed action sets its notice from the same cause `runAction` recorded, so
   * clearing only one would leave an identical toast behind and the click would
   * look as dead as the bug this closes. An error saying something DIFFERENT is
   * not what the user just dismissed — it survives and takes the toast's place,
   * which is what happened before this change and is the half worth keeping.
   * It reaches that state when a handler translates the failure it caught into
   * friendlier words, and when an error arrives while an older notice is still
   * up — nothing expires a notice.
   */
  /** The cleanup roles belonging to whichever copy of a failure the toast shows. */
  const toastCleanup = notice === undefined ? runtime.errorCleanup : noticeCleanup;

  const dismissToast = () => {
    if (notice === undefined || notice === runtime.error) runtime.clearError();
    setNotice(undefined);
  };

  /**
   * PRD #802 M2: every entry dispatches through `VOICE_ACTIONS` rather than
   * calling a setter of its own. The two generated groups register the KIND and
   * not the instance — one `focusAgent` entry taking the agent as a parameter,
   * however many agents the snapshot holds — because a guard cannot enumerate a
   * registry key per live agent.
   */
  const commandItems = [
    ...(coordinator ? [{ label: "Message coordinator…", hint: `Focus ${coordinator.displayName}'s terminal`, icon: Send, run: () => VOICE_ACTIONS.messageCoordinator.run(voiceContext, { agentId: coordinator.id }) }] : []),
    { label: "Manage projects", hint: "Choose repositories & workflows", icon: FolderGit2, run: () => VOICE_ACTIONS.openProjects.run(voiceContext) },
    { label: "Open prompt library", hint: "Reusable workflow launch prompts", icon: BookMarked, run: () => VOICE_ACTIONS.openPromptLibrary.run(voiceContext) },
    { label: "Open agent profiles", hint: "Configure models & permissions", icon: Bot, run: () => VOICE_ACTIONS.openAgentProfiles.run(voiceContext) },
    { label: "Edit workflow order", hint: "Enable, skip, or reorder roles", icon: Network, run: () => VOICE_ACTIONS.openWorkflowOrder.run(voiceContext) },
    { label: "Open settings", hint: "Appearance and other app preferences", icon: Settings2, run: () => VOICE_ACTIONS.openSettings.run(voiceContext) },
    { label: evidenceOpen ? "Hide evidence drawer" : "Show evidence drawer", hint: "Toggle transition evidence", icon: PanelRight, run: () => VOICE_ACTIONS.toggleEvidenceDrawer.run(voiceContext) },
    ...snapshot.agents.map((agent, index) => ({ label: `Focus ${agent.role}`, hint: `Shortcut ${index + 1}`, icon: SquareTerminal, run: () => VOICE_ACTIONS.focusAgent.run(voiceContext, { agentId: agent.id }) })),
    ...(mode === "fixture" ? [{ label: "Advance fixture", hint: "Move the deterministic loop one node", icon: Zap, run: () => VOICE_ACTIONS.advanceFixture.run(voiceContext) }] : []),
  ];

  return (
    <div className={`control-deck ${evidenceOpen ? "with-evidence" : ""}`}>
      <aside className="rail" aria-label="Primary navigation">
        <div className="brand-mark" aria-label="Agent Deck"><span>AD</span><i aria-hidden="true" /></div>
        <nav>
          {/* PRD #802 M2: every one of these dispatches through the action registry. */}
          <RailButton icon={FolderGit2} label="Projects" active={projectsOpen} onClick={() => VOICE_ACTIONS.openProjects.run(voiceContext)} testId="open-projects" />
          <RailButton icon={Activity} label="Runs" active={!projectsOpen && !workflowOpen && !profilesOpen && !promptsOpen && !settingsOpen} onClick={() => VOICE_ACTIONS.showRuns.run(voiceContext)} />
          {/* The one rail button that is a real view rather than an overlay toggle. */}
          <RailButton icon={LayoutList} label="Overview" onClick={() => VOICE_ACTIONS.openOverview.run(voiceContext)} testId="open-overview" />
          <RailButton icon={BookMarked} label="Prompts" active={promptsOpen} onClick={() => VOICE_ACTIONS.openPromptLibrary.run(voiceContext)} testId="open-prompts" />
          <RailButton icon={Network} label="Workflows" active={workflowOpen} onClick={() => VOICE_ACTIONS.openWorkflowOrder.run(voiceContext)} />
          <RailButton icon={Bot} label="Agent Profiles" active={profilesOpen} onClick={() => VOICE_ACTIONS.openAgentProfiles.run(voiceContext)} testId="open-agent-profiles" />
          <RailButton icon={Settings2} label="Settings" active={settingsOpen} onClick={() => VOICE_ACTIONS.openSettings.run(voiceContext)} testId="open-settings" />
        </nav>
        <div className="rail-bottom">
          <button aria-label="Keyboard shortcuts" title="Keyboard shortcuts" onClick={() => setHelpOpen(true)}><Keyboard size={18} /></button>
          {/* PRD #741 final audit F5: `connection.message` is daemon-supplied —
              it embeds the REMOTE daemon's `build_version`, which is an
              unvalidated string on the wire — and `safe_message` on the Rust
              side covers category `Cc` only, so the bidi controls reach here
              intact. Same seam treatment as every other daemon string on this
              screen. */}
          <span className={`connection-lamp connection-${snapshot.connection.status}`} title={snapshot.connection.message && displayText(snapshot.connection.message, DISPLAY_LIMITS.message)} />
        </div>
      </aside>

      <main className="deck-main">
        <header className="topbar">
          <div className="repo-context">
            <div className="repo-line"><FolderGit2 size={15} /><strong>{activeProject?.displayName || snapshot.repo}</strong><ChevronRight size={13} /><span>{snapshot.runId}</span></div>
            {/*
              PRD #745 M8: the branch chip appears only when there IS a branch.
              Nothing daemon-side tracks one, so live mode reports none and the
              line carries the working directory alone rather than printing the
              literal "Unavailable" where a branch name belongs.
            */}
            {/*
              `activeProject.displayPath` is the escaped twin of the DAEMON's
              canonical spelling of the project chosen for the next launch;
              `snapshot.worktree` is the daemon-reported cwd of a running agent.
              Both come from the daemon — the third tier this line used to fall
              back to was the desktop's own guess, and it is gone (PRD #819 M6).

              The DISPLAY half, not `path`. This line is a text node and a
              `title` attribute, so it gets the escaped copy; `path` is reserved
              for what goes back to the daemon (PRD #819 audit fix).
            */}
            <div className="branch-line">{snapshot.branch && <><GitBranch size={12} /><span>{snapshot.branch}</span><i /> </>}<span title={activeProject?.displayPath || snapshot.worktree}>{activeProject?.displayPath || snapshot.worktree}</span></div>
            {/*
              PRD #741 M9: the Deck selector, here rather than in a corner
              because this block is what the instruments beside it are about —
              and in the same place on the overview, so it reads as one control
              across both screens.
            */}
            <DeckSelector settings={settings} connection={snapshot.connection} />
          </div>
          <div className="run-instruments">
            <Instrument label="HEALTH" testId="run-health"><span className={`health-value health-${snapshot.health}`}><i />{snapshot.health}</span></Instrument>
            <Instrument label="NODE"><strong>{String(snapshot.currentNode).padStart(2, "0")}<em>/{String(snapshot.totalNodes).padStart(2, "0")}</em></strong></Instrument>
            {/* Em dash, this deck's established "not known": no daemon tracks an attempt count (PRD #745 M8). */}
            <Instrument label="ATTEMPT"><strong>{snapshot.currentAttempt === undefined ? "—" : String(snapshot.currentAttempt).padStart(2, "0")}</strong></Instrument>
            <Instrument label="ELAPSED"><strong>{snapshot.elapsed}</strong></Instrument>
            <Instrument label="SPEND"><strong>{mode === "fixture" ? `$${snapshot.spend.toFixed(2)}` : "—"}</strong></Instrument>
          </div>
          <div className="top-actions">
            <button className="command-trigger" onClick={() => setPaletteOpen(true)}><Search size={14} /><span>Command</span><kbd>⌘ K</kbd></button>
            <button
              className="button secondary compact"
              data-testid="pause-run"
              disabled={mode === "live" || snapshot.connection.status !== "connected"}
              title={mode === "live" ? "Whole-run pause is not yet exposed by the deck" : snapshot.paused ? "Resume fixture run" : "Pause fixture run"}
              onClick={() => void perform({ type: snapshot.paused ? "resume_run" : "pause_run" }, snapshot.paused ? "Fixture resumed." : "Fixture paused.")}
            >{snapshot.paused ? <Play size={14} /> : <Pause size={14} />}<span>{snapshot.paused ? "Resume" : "Pause"}</span></button>
            <button
              className="button danger compact"
              data-testid="stop-run"
              aria-label={selectedAgent ? `Stop ${selectedAgent.role}` : "Stop deck"}
              title={selectedAgent ? `Stop ${selectedAgent.role}` : canControlDaemon ? "Stop the local deck" : snapshot.connection.localOnlyReason ?? "Deck is not connected"}
              disabled={!selectedAgent && !canControlDaemon}
              onClick={requestStop}
            ><CircleStop size={14} /><span>Stop</span></button>
          </div>
        </header>

        {mode === "fixture" && (
          <div className="fixture-bar">
            <span><Sparkles size={13} /> DEMO DATA</span>
            <p>Deterministic run fixture · no external agents or files are being changed.</p>
            <button data-testid="fixture-advance" onClick={() => void perform({ type: "advance_fixture" })}>Advance fixture <ArrowRight size={13} /></button>
          </div>
        )}

        {/*
          The banner also stays up while CONNECTED with a build-stamp caveat
          (issue #801). Accepting the mismatch is not the same as it going away:
          the desktop crate deliberately keeps it in the connection message on
          the bypass path, and a banner keyed on `status` alone threw that away
          at the exact moment it started to matter — the session where the two
          builds actually differ.
        */}
        {(snapshot.connection.status !== "connected" || snapshot.connection.buildStampMismatchOnly || snapshot.connection.selectionFallback) && (
          <div className={`connection-banner connection-${snapshot.connection.status}`} role="alert">
            {snapshot.connection.status === "loading" ? <RefreshCw className="spin" size={16} /> : <ShieldAlert size={16} />}
            <div><strong>{snapshot.connection.status === "loading" ? "Establishing control channel" : snapshot.connection.status === "connected" ? (snapshot.connection.selectionFallback ? "Using the deck on this machine" : "Connected to a differently-built deck") : snapshot.connection.status === "error" ? "Desktop bridge error" : "Deck disconnected"}</strong><span data-testid="connection-banner-message">{snapshot.connection.message && displayText(snapshot.connection.message, DISPLAY_LIMITS.message)}</span>{/*
              PRD #741 M7. The stored selection could not be honoured, so the
              app is on the local deck — and it says which of the two reasons it
              was. This is why the banner's condition now includes it: a
              `NoRemoteSocket` fallback leaves the app CONNECTED, so without this
              the substitution would be silent, and acting on the wrong machine's
              agents is the outcome that makes it worth a row.
            */}{snapshot.connection.selectionFallback && <span data-testid="selection-fallback">{displayText(snapshot.connection.selectionFallback, DISPLAY_LIMITS.message)}</span>}{/* PRD #741 M7: why Start and Replace are absent, said once, where they would have been. */}{remoteDeck && snapshot.connection.localOnlyReason && <span data-testid="remote-deck-notice">{displayText(snapshot.connection.localOnlyReason, DISPLAY_LIMITS.message)}</span>}</div>
            {snapshot.connection.status !== "loading" && <div className="connection-actions">{mode === "live" && !remoteDeck && snapshot.connection.status === "disconnected" && <button className="button primary compact" data-testid="start-daemon" onClick={requestStartDaemon}><Play size={13} /> Start deck</button>}{mode === "live" && !remoteDeck && snapshot.connection.daemonDetected && snapshot.connection.status === "error" && snapshot.connection.runningAgentCount === 0 && <button className="button primary compact" data-testid="replace-daemon" onClick={requestRestartDaemon}><RefreshCw size={13} /> Replace deck</button>}{mode === "live" && snapshot.connection.status === "error" && snapshot.connection.buildStampMismatchOnly && <button className="button primary compact" data-testid="connect-anyway" onClick={requestConnectAnyway}><ShieldAlert size={13} /> Connect anyway</button>}<button className="button secondary compact" onClick={() => void runtime.reconnect()}><RefreshCw size={13} /> Reconnect</button></div>}
          </div>
        )}

        <section className="workflow-strip" aria-labelledby="workflow-title">
          <header><div><span className="section-kicker">RUN GRAPH</span><h1 id="workflow-title">Visible deterministic loop</h1></div><button onClick={() => setWorkflowOpen(true)}><SlidersHorizontal size={13} /> Edit loop</button></header>
          <div className="workflow-track">
            {orderedStages.length ? orderedStages.map((stage, index) => (
              <div className={`workflow-node node-${stage.status} ${stage.enabled ? "" : "is-disabled"}`} key={stage.id} data-testid={`workflow-node-${stage.id}`}>
                <div className="node-glyph">{stage.status === "passed" ? <Check size={14} /> : stage.status === "failed" ? <X size={14} /> : <span>{String(index + 1).padStart(2, "0")}</span>}</div>
                <div><strong>{stage.label}</strong><small>{stage.enabled ? (stage.attempt === undefined ? stage.status : `${stage.status} · att ${stage.attempt}`) : "skipped"}</small></div>
                {index < orderedStages.length - 1 && <i className="workflow-link" aria-hidden="true" />}
              </div>
            )) : <div className="workflow-empty">No workflow nodes reported. Open <button onClick={() => setWorkflowOpen(true)}>Edit loop</button> to inspect configuration.</div>}
          </div>
        </section>

        <HandoffRail handoffs={snapshot.handoffs} />

        <section className="workspace-section" aria-label="Agent terminals">
          <header className="workspace-header">
            <div><span className="section-kicker">AGENT DECK</span><h2>Live work surfaces</h2></div>
            <div className="workspace-tools"><span>{snapshot.agents.length} agents</span><span>{snapshot.agents.filter((agent) => agent.status === "running").length} active</span><button className={evidenceOpen ? "is-active" : ""} onClick={() => setEvidenceOpen((open) => !open)}><PanelRight size={14} /> Evidence</button></div>
          </header>
          {snapshot.connection.status === "loading" && !snapshot.agents.length ? <LoadingDeck /> : snapshot.agents.length ? (
            <div className="agent-grid">
              {/*
                PRD #1105 M3. Every tile goes through {@link AgentPaneFrame},
                including every one that is NOT open, so opening a pane is a
                change of attributes on an element that is already there rather
                than a different element in its place. That is what keeps the
                xterm instance — and with it the one-viewport-per-agent rule,
                which the module-level `terminalRegistry` needs rather than
                merely prefers: it is keyed by bare agent id, so a second live
                viewport would overwrite the first's registration and the
                first's unmount would then clean up nothing.
              */}
              {snapshot.agents.map((agent) => (
                <AgentPaneFrame
                  key={agent.id}
                  open={agent.id === paneAgentId}
                  panePresent={paneAgentId !== undefined}
                  agent={agent}
                  mode={mode}
                  selected={agent.id === selectedAgentId}
                  tab={tabs[agent.id] ?? "terminal"}
                  terminalFeed={runtime.terminalFeed}
                  evidence={snapshot.evidence}
                  inputResult={runtime.terminalInputResults?.[agentKey(agent.daemonId, agent.id)]}
                  terminalFocusToken={terminalFocus?.agentId === agent.id ? terminalFocus.token : 0}
                  onSelect={() => setSelectedAgentId(agent.id)}
                  onTabChange={(tab) => setTabs((current) => ({ ...current, [agent.id]: tab }))}
                  onTerminalInput={runtime.sendTerminalInput}
                  onTerminalResize={runtime.resizeTerminal}
                  appliedGeometry={runtime.appliedGeometry?.[agentKey(agent.daemonId, agent.id)]}
                  onEvidenceSelect={(id) => { setSelectedEvidenceId(id); setEvidenceOpen(true); }}
                  onRename={mode === "live" ? renameAgent : undefined}
                  /*
                    Opening SELECTS as well, which is the difference between the
                    overlay's `selected` being harmlessly degenerate and being
                    true: one pane is on screen, so it is the selected one. It
                    also settles `@media (max-width: 680px)`'s
                    `.agent-tile:not(.is-selected) { display: none }` for the
                    promoted tile without relying on a specificity race.
                  */
                  onOpen={onNavigate && (() => VOICE_ACTIONS.openAgent.run(voiceContext, { deckId: agent.daemonId, agentId: agent.id, from: "deck" }))}
                  onClose={onCloseAgent && (() => VOICE_ACTIONS.closeAgentView.run(voiceContext))}
                />
              ))}
            </div>
          ) : <EmptyDeck onReconnect={() => void runtime.reconnect()} onProfiles={() => setProfilesOpen(true)} />}
        </section>
      </main>

      {evidenceOpen && <EvidenceDrawer evidence={snapshot.evidence} selected={selectedEvidence} onSelect={setSelectedEvidenceId} onClose={() => setEvidenceOpen(false)} />}

      <ProjectsPanel
        open={projectsOpen}
        state={projectState}
        onClose={() => setProjectsOpen(false)}
        onConfigureWorkflow={() => { setProjectsOpen(false); setWorkflowOpen(true); }}
      />
      <PromptLibraryPanel
        open={promptsOpen}
        prompts={prompts}
        selectedId={selectedPromptId}
        onSelect={setSelectedPromptId}
        onClose={() => setPromptsOpen(false)}
        onAdd={() => setSelectedPromptId(addPrompt())}
        onUpdate={updatePrompt}
        onRemove={(id) => { removePrompt(id); setNotice("Prompt removed from this device's library."); }}
      />
      <ProfilesPanel open={profilesOpen} profiles={profiles} onClose={() => setProfilesOpen(false)} onUpdate={updateProfile} onReset={resetProfiles} onSaved={() => setNotice("Agent profile draft saved locally. Project TOML is unchanged.")} />
      <WorkflowPanel key={activeProject?.path ?? "runtime-workflow"} open={workflowOpen} profiles={profiles} order={profileOrder} mode={mode} project={activeProject} onChooseProject={() => { setWorkflowOpen(false); setProjectsOpen(true); }} onClose={() => setWorkflowOpen(false)} onToggle={(id) => { const profile = profiles.find((item) => item.id === id); if (profile) updateProfile(id, { enabled: !profile.enabled }); }} onMove={moveStage} onLaunch={requestLaunch} platformIssue={workflowPlatformIssue} capabilityIssue={snapshot.connection.projectActionsReason} prompts={prompts} />
      {/*
        PRD #741 M10. The provider is mounted HERE rather than inside the sheet,
        because `SettingsSheet` is a #803-owned rendering component and the
        contract it keeps is that a feature adding a section never opens it. The
        deck already holds the runtime, so this is where the bridge is; see
        `lib/settingsBridge.tsx` for why it is a context and not a fifth panel
        prop.
      */}
      <SettingsBridgeProvider value={settingsBridge}>
        <SettingsSheet
          open={settingsOpen}
          onClose={() => setSettingsOpen(false)}
          settings={settings.settings}
          onSave={settings.save}
          saveError={settings.saveError}
          path={settings.path}
          loaded={settings.loaded}
          mode={mode}
        />
      </SettingsBridgeProvider>
      {paletteOpen && <CommandPalette commands={commandItems} onClose={() => setPaletteOpen(false)} />}
      {helpOpen && <ShortcutHelp onClose={() => setHelpOpen(false)} />}
      {confirm && <ConfirmDialog state={confirm} onClose={() => setConfirm(undefined)} />}
      {/* PRD #1223 audit V7: the roles a rollback could not confirm are shown
          above the sentence, from whichever half is on screen. The overview
          mounts its own copy of this over `runtime.error` alone (audit W2) —
          it has no notice of its own, and it is not mounted at the same time
          as this one. */}
      {(notice || runtime.error) && <Toast message={notice ?? runtime.error ?? ""} cleanup={toastCleanup} onDismiss={dismissToast} />}
    </div>
  );
}

/**
 * The one message surface either screen shows: a failed action's sentence, the
 * roles a rollback could not confirm are stopped above it, and a dismiss.
 *
 * One component rather than markup per screen (PRD #1223 audit W2). The deck
 * had the only copy, and the overview mounts INSTEAD of the deck — so a launch
 * that failed after the New agent dialog was gone, which is the case the
 * runtime holds these roles for at all, was reported on a screen the user was
 * no longer on. The overview's Refresh then cleared it unseen.
 *
 * The message goes through `displayText` like every other daemon-influenced
 * string here, because a role name reaches it inside the failure sentence.
 */
function Toast({ message, cleanup, onDismiss }: { message: string; cleanup?: readonly string[]; onDismiss: () => void }) {
  return (
    <div className="toast" data-testid="toast" role="status">
      <AlertTriangle size={15} />
      <div className="toast-body">
        {cleanup && cleanup.length > 0 && <CleanupWarning stops={cleanup} testId="toast-cleanup-warning" />}
        <span>{displayText(message, DISPLAY_LIMITS.message)}</span>
      </div>
      <button aria-label="Dismiss message" onClick={onDismiss}><X size={14} /></button>
    </div>
  );
}

function RailButton({ icon: Icon, label, active, onClick, testId }: { icon: typeof Activity; label: string; active?: boolean; onClick: () => void; testId?: string }) {
  return <button className={active ? "is-active" : ""} aria-current={active ? "page" : undefined} title={label} onClick={onClick} data-testid={testId}><Icon size={18} /><span>{label}</span></button>;
}

function Instrument({ label, children, testId }: { label: string; children: ReactNode; testId?: string }) {
  return <div className="instrument" data-testid={testId}><span>{label}</span>{children}</div>;
}

function EvidenceDrawer({ evidence, selected, onSelect, onClose }: { evidence: EvidenceItem[]; selected?: EvidenceItem; onSelect: (id: string) => void; onClose: () => void }) {
  return (
    <aside className="evidence-drawer" data-testid="evidence-drawer" aria-label="Transition evidence">
      <header><div><span className="section-kicker">EVENT LEDGER</span><h2>Transition evidence</h2></div><button aria-label="Close evidence drawer" onClick={onClose}><X size={16} /></button></header>
      <div className="evidence-filter"><button className="is-active">All <span>{evidence.length}</span></button><button>Failures <span>{evidence.filter((item) => item.verdict === "FIX" || item.verdict === "ERROR").length}</span></button></div>
      <div className="evidence-list" role="listbox" aria-label="Run evidence">
        {evidence.length ? evidence.map((item) => (
          <button key={item.id} role="option" aria-selected={item.id === selected?.id} className={item.id === selected?.id ? "is-active" : ""} onClick={() => onSelect(item.id)}>
            <span className={`verdict verdict-${item.verdict.toLowerCase()}`}>{item.verdict}</span>
            <div><strong>{item.title}</strong><small>{item.to ? <>{item.from} <ArrowRight size={10} /> {item.to}</> : item.from}</small></div>
            <time>{item.at}</time>
          </button>
        )) : <div className="evidence-empty"><History size={20} /><strong>No events yet</strong><span>Live hook and handoff events appear here as agents work — delegations, deliveries, failures, and work-done reports included.</span></div>}
      </div>
      {selected && (
        <div className="evidence-detail">
          <div className="evidence-detail-head"><span className={`verdict verdict-${selected.verdict.toLowerCase()}`}>{selected.verdict}</span><span>{selected.acknowledged ? <><CheckCircle2 size={13} /> acknowledged</> : "unread"}</span></div>
          <h3>{selected.title}</h3>
          <p>{selected.summary}</p>
          <dl><div><dt>SENDER</dt><dd>{selected.from}</dd></div><div><dt>RECEIVER</dt><dd>{selected.to || "—"}</dd></div>{selected.command && <div className="wide"><dt>COMMAND</dt><dd><code>{selected.command}</code></dd></div>}{selected.exitCode !== undefined && <div><dt>EXIT</dt><dd>{selected.exitCode}</dd></div>}</dl>
          <div className="why-edge"><Gauge size={14} /><div><strong>{selected.to ? "Why this edge opened" : "Where this came from"}</strong><p>{selected.reason}</p></div></div>
        </div>
      )}
      <footer><span><i className="status-running" /> append-only run ledger</span><kbd>J</kbd><kbd>K</kbd></footer>
    </aside>
  );
}

function LoadingDeck() {
  return <div className="loading-grid" aria-label="Loading agents">{[0, 1, 2, 3].map((item) => <div key={item}><span /><i /><i /><b /></div>)}</div>;
}

function EmptyDeck({ onReconnect, onProfiles }: { onReconnect: () => void; onProfiles: () => void }) {
  return <div className="empty-deck"><Blocks size={28} /><h3>No active agent surfaces</h3><p>Connect to a running deck or prepare agent profiles before starting the loop.</p><div><button className="button secondary" onClick={onProfiles}><Bot size={14} /> Configure agents</button><button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button></div></div>;
}

function CommandPalette({ commands, onClose }: { commands: { label: string; hint: string; icon: typeof Bot; run: () => void }[]; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const filtered = commands.filter((item) => `${item.label} ${item.hint}`.toLowerCase().includes(query.toLowerCase()));
  return <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section className="command-palette" role="dialog" aria-modal="true" aria-label="Command menu" onMouseDown={(event) => event.stopPropagation()}><label><Search size={17} /><input autoFocus placeholder="Search controls, agents, and views…" value={query} onChange={(event) => setQuery(event.target.value)} /><kbd>ESC</kbd></label><div>{filtered.map(({ label, hint, icon: Icon, run }) => <button key={label} onClick={() => { run(); onClose(); }}><Icon size={16} /><span><strong>{label}</strong><small>{hint}</small></span><ChevronRight size={14} /></button>)}{!filtered.length && <p>No matching controls.</p>}</div><footer><span><Command size={12} /> local control surface</span><span><kbd>↵</kbd> select</span></footer></section></div>;
}

function ShortcutHelp({ onClose }: { onClose: () => void }) {
  const shortcuts = [["⌘ K", "Command menu"], ["⌘/Ctrl + / − / 0", "Zoom in, out, reset"], ["1 — 4", "Focus agent"], ["J / K", "Move through evidence"], ["?", "Shortcut guide"], ["ESC", "Close overlay"]];
  return <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section className="shortcut-dialog" role="dialog" aria-modal="true" aria-labelledby="shortcut-title" onMouseDown={(event) => event.stopPropagation()}><header><div><HelpCircle size={18} /><h2 id="shortcut-title">Control keys</h2></div><button aria-label="Close shortcut guide" onClick={onClose}><X size={16} /></button></header>{shortcuts.map(([keys, label]) => <div key={keys}><span>{label}</span><kbd>{keys}</kbd></div>)}</section></div>;
}
