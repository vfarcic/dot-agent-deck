import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
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
import { SettingsBridgeProvider } from "./lib/settingsBridge";
import { DISPLAY_LIMITS, deckName, displayText } from "./lib/displayText";
import { useAgentProfiles } from "./hooks/useAgentProfiles";
import { useDeckRuntime } from "./hooks/useDeckRuntime";
import { useDaemonProjects } from "./hooks/useDaemonProjects";
import { usePromptLibrary } from "./hooks/usePromptLibrary";
import { useDesktopSettings, type DesktopSettingsState } from "./hooks/useDesktopSettings";
import { useInertBackground } from "./hooks/useInertBackground";
import { useShownTerminals } from "./hooks/useShownTerminals";
import { useZoom } from "./hooks/useZoom";
import { agentKey } from "./lib/agentKey";
import { otherDeckTerminalState } from "./lib/terminalInput";
import { applyAppearance } from "./lib/appearance";
import { desktopWorkflowPlatformIssue } from "./lib/platform";
import type { DeckAction, DeckRuntimeState, DeckView, EvidenceItem, PanelTab, WorkflowLaunchConfig } from "./types";
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
 * five of the rail's six buttons are overlay toggles over an always-mounted
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
  const agentView = view.kind === "agent" ? view : undefined;
  /**
   * Back, and the whole of it. The destination is read off the view rather
   * than popped from a stack, so an agent view that was never navigated TO —
   * the app's `initialView`, a future deep link — closes to a real screen
   * instead of to nothing.
   */
  const closeAgent = useCallback(() => setView((current) => (current.kind === "agent" ? { kind: current.from } : current)), []);
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
      if (event.key === "Escape") closeAgent();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [agentView, closeAgent]);
  const base = agentView?.from ?? view.kind;
  const selectedDeckId = runtime.snapshot.connection.deckId;
  /**
   * PRD #1105 — whether the open pane's agent is on the deck that is IN FORCE,
   * which is the one condition two different things read.
   *
   * It decides whether an attach may be declared for that agent, and it decides
   * whether the pane shows a terminal or says why it has none. Those must agree
   * — a pane showing a terminal nothing attached to is the black rectangle this
   * feature exists to replace, and a pane explaining itself while an attach is
   * live would be explaining a state it is not in. One expression is how they
   * are kept in agreement; two comparisons of the same two values is how they
   * would drift.
   *
   * **The comparison is load-bearing rather than defensive, and since every
   * listed agent is openable it is now reachable on the ordinary path.** Attach
   * targets whichever deck is linked at the instant it runs (`terminal::attach`
   * takes `trusted_daemon`), and agent ids are per-daemon monotonic integers —
   * so declaring `[agentId]` while another deck is in force attaches *that*
   * deck's agent of the same id, on another machine, under the right name, and
   * routes this client's keystrokes to it. The fleet fixture has a `planner` on
   * two decks, which is the ordinary case and not a contrived one. Opening a
   * non-selected deck's agent from the overview reaches this directly; so does
   * the selected deck moving under an already-open pane for reasons that are
   * nobody's gesture — a `selectionFallback` the crate reports, another window
   * writing the settings document, a reconnect.
   *
   * An UNKNOWN selected deck matches nothing. The loading and error seeds in
   * `useDeckRuntime` carry no `deckId`, and `agentView.deckId` is a string, so
   * the strict comparison lands on "not attached" with no special case — which
   * is the safe direction: an app that cannot name the deck in force must not
   * promise an attach.
   */
  const paneDeckSelected = agentView !== undefined && agentView.deckId === selectedDeckId;
  /**
   * PRD #1105 M4 — the shown set for the OVERVIEW tree, declared here because
   * this is the only component that can see the overview and the pane over it
   * in one commit. `undefined` on the deck path hands ownership to
   * {@link DeckSurface} without declaring anything; see {@link useShownTerminals}.
   *
   * An agent is declared shown only once its deck IS the selected one, per
   * `paneDeckSelected` above. So a pane over a non-selected deck's agent costs
   * **no** attach at all, where one over the selected deck's costs exactly one.
   */
  const overviewShown = base === "overview"
    ? (agentView && paneDeckSelected ? [agentView.agentId] : [])
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
  if (base === "overview") {
    return (
      <>
        <AgentOverview runtime={runtime} settings={settings} onNavigate={setView} />
        {/*
          The overview mounts no terminal of its own (PRD #745's commitment), so
          there is no tile here to promote and the pane is a sibling of the
          screen rather than a promotion inside it. That still leaves exactly
          one live `TerminalViewport` for the agent, which is the property M3
          actually requires.
        */}
        {agentView && <OverviewAgentPane runtime={runtime} view={agentView} attached={paneDeckSelected} onClose={closeAgent} />}
      </>
    );
  }
  /* The COMPOSITE identity, never the bare id. See `deckPaneRetargeted` above
     and `DeckSurface`'s own promotion condition. */
  const openAgent = agentView ? { deckId: agentView.deckId, agentId: agentView.agentId } : undefined;
  return <DeckSurface runtime={runtime} settings={settings} workflowPlatformIssue={workflowPlatformIssue} onNavigate={setView} openAgent={openAgent} onCloseAgent={closeAgent} />;
}

/**
 * The pane over the OVERVIEW, with the tile state the deck would otherwise
 * have owned.
 *
 * Split out for the hook, not for the rendering: the panel tab is component
 * state and the agent lookup can fail, so the two cannot live in
 * {@link DeckShell}'s body without either a conditional hook or a `tabs` map
 * kept for a screen that has no tiles.
 *
 * An agent that is not in the fleet renders nothing — the deck can retire a
 * pane while its overlay is open, and PRD #1105 records what that should LOOK
 * like as an open question. `Escape` still closes the view, because that
 * listener is {@link DeckShell}'s and not this component's.
 *
 * # The lookup is by the COMPOSITE identity, not by the bare id
 *
 * PRD #1105 M6. This screen merges every observed deck and every agent it lists
 * is openable, so this pane can be for an agent on a deck that is not the
 * selected one — and the selected deck's snapshot is exactly where a bare-id
 * lookup would find a *different* agent wearing the same per-daemon monotonic
 * id. Resolving against the fleet entry named by `view.deckId` is what the
 * variant carries a `deckId` for, and it is what keeps the pane on the agent
 * the user opened when the selection moves under it.
 *
 * # `attached` is the pane's whole deck story, and it is a state rather than a
 * refusal
 *
 * A terminal in this app is always the *selected* deck's, so a pane for an
 * agent on another deck attaches nothing — {@link DeckShell}'s
 * `paneDeckSelected` is the one condition that decides both that and this, so
 * the two cannot disagree. The pane opens either way: everything that is a
 * property of the AGENT works — header, status, prompt, tool, all five panel
 * tabs, `Esc` and the close control — and the terminal tab renders an explicit
 * no-terminal state naming the deck instead of a `TerminalViewport` that would
 * receive no bytes.
 *
 * Saying it is the right half of the job; doing something about it is not this
 * pane's. Switching the selected deck on open was built and withdrawn under
 * this PRD (decision 5), and a control here that switches decks is
 * [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073)'s design
 * question.
 */
function OverviewAgentPane({ runtime, view, attached, onClose }: { runtime: DeckRuntimeState; view: Extract<DeckView, { kind: "agent" }>; attached: boolean; onClose: () => void }) {
  const [tab, setTab] = useState<PanelTab>("terminal");
  const deck = runtime.fleet.find((entry) => entry.connection.deckId === view.deckId);
  const agent = deck?.agents.find((candidate) => candidate.id === view.agentId);
  if (!deck || !agent) return null;
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
         the user just came from calls it. */
      noTerminal={attached ? undefined : otherDeckTerminalState(deckName(deck.connection))}
      /* This deck's, for the same reason the agent above is: the selected
         deck's ring belongs to a different machine until the M6 switch lands,
         and the bridge records events for the selected deck alone — so a
         non-selected deck's entry carries none rather than somebody else's. */
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

export function DeckSurface({ runtime, settings, workflowPlatformIssue = desktopWorkflowPlatformIssue(), onNavigate, openAgent, onCloseAgent }: { runtime: DeckRuntimeState; settings: DesktopSettingsState; workflowPlatformIssue?: string; onNavigate?: (view: DeckView) => void; openAgent?: { deckId: string; agentId: string }; onCloseAgent?: () => void }) {
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
  const [notice, setNotice] = useState<string>();
  const [confirm, setConfirm] = useState<ConfirmState>();
  // Memoised so the context value is stable across renders; `runtime.testEndpoint`
  // is itself stable for the lifetime of the bridge.
  const settingsBridge = useMemo(() => ({ testEndpoint: runtime.testEndpoint }), [runtime.testEndpoint]);
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
    .map((agent) => agent.id);

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

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
        return;
      }
      if (event.key === "Escape") {
        setPaletteOpen(false); setHelpOpen(false); setProjectsOpen(false); setProfilesOpen(false); setPromptsOpen(false); setWorkflowOpen(false); setSettingsOpen(false); setConfirm(undefined);
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
        if (agent) setSelectedAgentId(agent.id);
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
      action: async () => { await perform({ type: "stop_agent", agentId: selectedAgent.id }, `${selectedAgent.role} stop requested.`); },
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
  const dismissToast = () => {
    if (notice === undefined || notice === runtime.error) runtime.clearError();
    setNotice(undefined);
  };

  const commandItems = [
    ...(coordinator ? [{ label: "Message coordinator…", hint: `Focus ${coordinator.displayName}'s terminal`, icon: Send, run: () => focusTerminal(coordinator.id) }] : []),
    { label: "Manage projects", hint: "Choose repositories & workflows", icon: FolderGit2, run: () => setProjectsOpen(true) },
    { label: "Open prompt library", hint: "Reusable workflow launch prompts", icon: BookMarked, run: () => setPromptsOpen(true) },
    { label: "Open agent profiles", hint: "Configure models & permissions", icon: Bot, run: () => setProfilesOpen(true) },
    { label: "Edit workflow order", hint: "Enable, skip, or reorder roles", icon: Network, run: () => setWorkflowOpen(true) },
    { label: "Open settings", hint: "Appearance and other app preferences", icon: Settings2, run: () => setSettingsOpen(true) },
    { label: evidenceOpen ? "Hide evidence drawer" : "Show evidence drawer", hint: "Toggle transition evidence", icon: PanelRight, run: () => setEvidenceOpen((open) => !open) },
    ...snapshot.agents.map((agent, index) => ({ label: `Focus ${agent.role}`, hint: `Shortcut ${index + 1}`, icon: SquareTerminal, run: () => setSelectedAgentId(agent.id) })),
    ...(mode === "fixture" ? [{ label: "Advance fixture", hint: "Move the deterministic loop one node", icon: Zap, run: () => { void perform({ type: "advance_fixture" }); } }] : []),
  ];

  return (
    <div className={`control-deck ${evidenceOpen ? "with-evidence" : ""}`}>
      <aside className="rail" aria-label="Primary navigation">
        <div className="brand-mark" aria-label="Agent Deck"><span>AD</span><i aria-hidden="true" /></div>
        <nav>
          <RailButton icon={FolderGit2} label="Projects" active={projectsOpen} onClick={() => setProjectsOpen(true)} testId="open-projects" />
          <RailButton icon={Activity} label="Runs" active={!projectsOpen && !workflowOpen && !profilesOpen && !promptsOpen && !settingsOpen} onClick={() => { setProjectsOpen(false); setWorkflowOpen(false); setProfilesOpen(false); setPromptsOpen(false); setSettingsOpen(false); }} />
          {/* The one rail button that is a real view rather than an overlay toggle. */}
          <RailButton icon={LayoutList} label="Overview" onClick={() => onNavigate?.({ kind: "overview" })} testId="open-overview" />
          <RailButton icon={BookMarked} label="Prompts" active={promptsOpen} onClick={() => setPromptsOpen(true)} testId="open-prompts" />
          <RailButton icon={Network} label="Workflows" active={workflowOpen} onClick={() => setWorkflowOpen(true)} />
          <RailButton icon={Bot} label="Agent Profiles" active={profilesOpen} onClick={() => setProfilesOpen(true)} testId="open-agent-profiles" />
          <RailButton icon={Settings2} label="Settings" active={settingsOpen} onClick={() => setSettingsOpen(true)} testId="open-settings" />
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
                  onOpen={onNavigate && (() => {
                    setSelectedAgentId(agent.id);
                    onNavigate({ kind: "agent", deckId: agent.daemonId, agentId: agent.id, from: "deck" });
                  })}
                  onClose={onCloseAgent}
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
      {(notice || runtime.error) && <div className="toast" data-testid="toast" role="status"><AlertTriangle size={15} /><span>{notice ?? runtime.error}</span><button aria-label="Dismiss message" onClick={dismissToast}><X size={14} /></button></div>}
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
