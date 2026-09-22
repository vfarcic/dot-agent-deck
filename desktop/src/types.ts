export type RuntimeMode = "fixture" | "live";

/**
 * The control deck's legacy stand-in for a value the daemon did not report.
 *
 * It is a WORD A SCREEN PRINTS, never a value the model carries for `cwd`. It
 * was both until the M8 audit: `agentFromDto` substituted it for an unreported
 * `cwd` and `toOverviewAgent` reversed it, which made the two sides agree with
 * each other but not with the daemon — `src/agent_pty.rs` accepts any
 * non-empty, bounded, control-free `cwd`, so `"Unavailable"` is a perfectly
 * legal directory name, and an agent launched in one had its real, reported
 * directory silently erased into a blank cell with no hover text. A sentinel
 * spelled in the same alphabet as the data can always be spelled BY the data;
 * `cwd` is optional now, and absence is the thing that cannot be spelled.
 *
 * What remains here is the two FIXTURE-ONLY fields live mode has no source for
 * at all — `model` and `worktree` — plus `AgentTile`'s own footer, which turns
 * an absent `cwd` back into this word at the moment it prints it. No surface
 * matches on it any more.
 */
export const UNREPORTED = "Unavailable";

export type ConnectionStatus = "loading" | "connected" | "disconnected" | "error";
export type RunHealth = "healthy" | "attention" | "failed" | "idle";
export type AgentStatus = "queued" | "running" | "waiting" | "passed" | "failed" | "stopped";
export type StageStatus = "queued" | "active" | "passed" | "failed" | "waiting";
export type PanelTab = "terminal" | "diff" | "checks" | "handoffs" | "artifacts";

/**
 * Which of `AgentTile`'s two presentations to render (PRD #1105 M1).
 *
 * The grid tile and the agent-pane overlay are the SAME component at two
 * sizes, and this is the one prop the differences are derived from — the whole
 * point of the milestone that introduced it is that there is no
 * `AgentTileOverlay`. A string union rather than an `isOverlay` boolean for
 * three reasons a boolean cannot give: a third presentation (#745's deferred
 * group view) arrives as a member rather than as a second boolean, whose four
 * combinations include one that means nothing; `grep -rn '"overlay"'` finds
 * every derivation; and where a derivation is written as a total map or a
 * checked `switch`, an added member fails to compile instead of silently
 * taking a `false` branch.
 *
 * The values name WHERE the pane appears, not how big it is, because not every
 * difference is dimensional — the Reader launcher's fate is an affordance
 * question, not a size one.
 */
export type AgentPanePresentation = "tile" | "overlay";

export type Verdict = "PASS" | "FIX" | "HUMAN" | "ERROR" | "INFO";

export interface ConnectionView {
  status: ConnectionStatus;
  /**
   * This deck's KEY — the crate's `EndpointIdentity::wire_id()`, an opaque
   * `deck-<16 hex>` token (PRD #742 M5). Every per-deck map, React key and
   * `agentKey` composite is built on it.
   *
   * Optional because the loading and error seeds in `useDeckRuntime` have no
   * deck to name yet — they are placeholders for a fleet that has not arrived.
   * Every deck that came off the wire or out of the fixture carries one, so a
   * consumer keying on it should treat `undefined` as "not a deck", never as a
   * value to fall back to `socketPath` from: falling back is precisely the
   * collision this field exists to remove.
   */
  deckId?: string;
  /**
   * What this deck is CALLED — the crate's `Endpoint::describe()`: a socket path
   * for a local deck, `user@host[:port]` for a remote one.
   *
   * **A label.** It is what the overview prints and what a hover discloses, and
   * it is deliberately NOT unique: two daemons on one host describe identically.
   * `deckId` above is what anything keying on a deck uses.
   */
  socketPath?: string;
  /**
   * This deck is CONFIGURED but has no address yet (PRD #742 M12) — a stored
   * row whose socket path `Test connection` has not filled in.
   *
   * Set only by `unconfiguredDeckSnapshot`, which builds the entry the crate's
   * `DesktopSnapshotDto.unconfigured` describes. It is not a connection state:
   * nothing was contacted and nothing failed, which is exactly why the
   * `disconnected` note — "no deck is listening", "start one, then reconnect" —
   * is the wrong sentence for it and the overview renders its own.
   */
  unconfigured?: boolean;
  /**
   * This deck is in the fleet and HAS NOT REPORTED YET (PRD #742 M14) — a
   * fourth honest state beside connected, disconnected and unconfigured.
   *
   * Set in live mode by `pendingDeckSnapshot` alone, from an entry the crate
   * states on `DesktopSnapshotDto.observed` that no snapshot has arrived for —
   * and by the `fleet` fixture, which holds the state still so it can be
   * looked at. A deck
   * joins the fleet when the settings document is applied and emits its own
   * snapshot only once its watcher has a tunnel, a handshake and an agent
   * list — up to `FORWARD_READY_TIMEOUT` (30s) for a remote deck — so without
   * this the fleet's own TOTAL climbed while the reader watched: `1/1`, then
   * `2/2` a few seconds later, both reading as "everything is fine" and only
   * one of them true.
   *
   * `status` is `"loading"` and never `"disconnected"`, which is the whole
   * distinction: disconnected asserts that something was asked and nothing
   * answered, and here nothing has been asked yet. The flag is what separates
   * it from `useDeckRuntime`'s pre-connect loading seed, which is the app
   * having no deck rather than a deck having no snapshot.
   */
  pending?: boolean;
  message?: string;
  /**
   * Which kind of deck this connection is to (PRD #741 M7): `"local"` for a
   * daemon on this machine, `"remote"` for one reached over an ssh tunnel.
   *
   * Defaulted to `"local"` where a fixture or an older reply says nothing, so a
   * screen that gates a destructive control on it fails safe in the direction
   * that keeps today's behaviour rather than in the direction that enables a
   * button against a machine the user does not own.
   */
  deckKind?: "local" | "remote";
  /**
   * Why the daemon-lifecycle controls are unavailable, when they are — present
   * exactly when `deckKind` is `"remote"`.
   *
   * `Endpoint::require_local("Stop daemon")`'s own sentence, not one written on
   * this side. Stop and Replace act on a process on *this* machine, and over a
   * forwarded socket `run_daemon_stop`'s peer-credential lookup names the local
   * `ssh` client — so pressing Stop would tear the tunnel down and report that a
   * daemon had stopped gracefully.
   */
  localOnlyReason?: string;
  /**
   * Why the app is talking to the local deck when the stored selection named
   * another one (PRD #741 M7). "That deck is gone" and "that deck has no socket
   * path yet" are different things to tell a user, and neither is "connected to
   * local".
   */
  selectionFallback?: string;
  /**
   * Why the project-aware surfaces — choosing a project, preparing and launching
   * a workflow — are unavailable against this deck (PRD #741 M8).
   *
   * The daemon's own sentence, derived from what it ADVERTISED in its `Hello`
   * reply rather than from a version number or a build stamp. Absent means
   * available, so a screen reads absence as "nothing to say" and not as "unknown
   * — better disable it": the desktop crate omits the field only when every verb
   * the launch needs was advertised.
   */
  projectActionsReason?: string;
  /** True when a daemon answered Hello but failed protocol/build compatibility. */
  daemonDetected?: boolean;
  /** Honest count reported by Hello; undefined when the daemon could not report it. */
  runningAgentCount?: number;
  /**
   * True when the ONLY thing that failed the handshake is the git-describe build
   * stamp: the wire protocol agreed on both sides, so proceeding is a judgement
   * the user may legitimately make and **Connect anyway** is offered (issue
   * #801). A protocol mismatch never sets it — that check runs first in the
   * desktop crate and is not overridable from anywhere, so a screen must never
   * offer a button for it.
   */
  buildStampMismatchOnly?: boolean;
  /**
   * The two git-describe stamps, carried so the fact that the builds differ
   * stays DISCOVERABLE without being an alert.
   *
   * Since issue #801 the crate connects silently when both stamps name the same
   * release, because by this project's bump policy no compatibility break sits
   * between them — so there is no banner and no message to read. These two are
   * what a hover can still show. Absent in fixture mode, and the daemon's is
   * absent whenever it reported none.
   */
  clientBuildVersion?: string;
  daemonBuildVersion?: string;
}

/**
 * One project the DAEMON knows about (PRD #819 M6).
 *
 * It replaced `DeckProject`, which was a locally-invented record — a minted id,
 * a free-typed `cwd`, a workflow name, notes — persisted under
 * `dot-agent-deck.desktop.projects.v1` and used as the source of truth for the
 * launch working directory. Nothing validated it against the daemon's world, so
 * against a remote daemon it named a directory on the wrong machine.
 *
 * There is no id, because the daemon-canonical `path` IS the identity, and no
 * editable fields, because nothing here is stored: a project is a property of
 * the launch being assembled and dies with it.
 */
export interface DaemonProject {
  /**
   * Daemon-canonical absolute path, byte for byte. Sent back verbatim; never
   * re-spelled, and never rendered — render `displayPath` instead.
   *
   * PRD #819 audit fix (P2, finding 1): the desktop crate used to escape and
   * truncate this value and keep the result as the only copy, so a valid
   * canonical path carrying a control character, or longer than 2048
   * characters, was submitted as a *different* path. Identity and display are
   * now two fields.
   */
  path: string;
  /** `path`, escaped and bounded for rendering. Never sent anywhere. */
  displayPath: string;
  /** The daemon's basename for this project, escaped for rendering. */
  displayName: string;
}

export interface DaemonProjectListing {
  projects: DaemonProject[];
  /**
   * The project most recently active on this daemon — a fact derived from live
   * state, not a remembered preference. Absent when the daemon has nothing
   * live, which is the empty state rather than an error.
   */
  primary?: string;
}

export interface DaemonOrchestrationRole {
  /** Identity: matched by name at the launch, and it becomes the pane's label. */
  name: string;
  /** `name`, escaped for rendering. */
  displayName: string;
  start: boolean;
}

export interface DaemonOrchestration {
  /** Identity: this exact string goes back as the launch's workflow name. */
  name: string;
  /** `name`, escaped for rendering. */
  displayName: string;
  default: boolean;
  roles: DaemonOrchestrationRole[];
}

/**
 * One resolved project: the canonical path, and the workflows that project
 * offers. The order is `daemon → project → workflow` and it is not
 * rearrangeable — the workflow list comes out of the project's own config, so
 * there is nothing to offer before a project is chosen.
 */
export interface DaemonResolvedProject {
  /** Identity: the canonical spelling the launch must use, byte for byte. */
  path: string;
  /** `path`, escaped and bounded for rendering. Never sent anywhere. */
  displayPath: string;
  /** The canonical path's basename, escaped for rendering. */
  displayName: string;
  orchestrations: DaemonOrchestration[];
  /**
   * The revision these orchestrations were read from, echoed back on the launch
   * so a config edited in between is refused rather than silently launched
   * against.
   */
  configRevision?: string;
}

export interface DeckPrompt {
  id: string;
  name: string;
  body: string;
  note?: string;
}

export interface WorkflowStage {
  id: string;
  label: string;
  agentId?: string;
  status: StageStatus;
  /**
   * FIXTURE-ONLY, and optional for the same reason as `AgentSession.attempt`:
   * live mode derived it from that hardcoded `1`, so every node claimed an
   * attempt count no daemon tracks (PRD #745 M8).
   */
  attempt?: number;
  enabled: boolean;
}

export interface CheckResult {
  id: string;
  name: string;
  status: "passed" | "failed" | "running" | "queued";
  duration?: string;
  command?: string;
}

export interface Artifact {
  id: string;
  name: string;
  kind: "file" | "report" | "recording";
  path: string;
}

/**
 * Which top-level surface is mounted. A discriminated union from the start even
 * though it carried only two variants for its first two PRDs, so PRD #745
 * iteration 3's group and single-agent views arrive as added variants rather
 * than as a refactor of a boolean. No router library is warranted for this.
 */
export type DeckView =
  | { kind: "deck" }
  | { kind: "overview" }
  /**
   * PRD #1105 M2 — one agent's pane, OVER the screen it was opened from.
   *
   * The first two variants REPLACE the mounted screen, and `DeckShell` says so
   * in its own doc comment. This one deliberately does not: an overlay that
   * unmounted the screen beneath it would re-declare that screen's shown
   * terminal set, and a nine-tile deck coming back costs five re-attaches and
   * five scrollback replays. So `from` names the base screen to keep mounted
   * underneath, and closing is `setView({ kind: from })`.
   *
   * `from` is also the ONLY record of where back goes. There is no history
   * stack and none is wanted: a view reachable from two screens has to carry
   * which one it came from anyway, and carrying it in the value makes a direct
   * initial agent view — one with no prior navigation at all — close to the
   * right place by construction.
   *
   * `deckId` is here even though nothing in M2/M3/M5 reads it, because the
   * overview merges every observed deck's agents and an agent id is per-daemon
   * monotonic: `agentId` alone names an agent on the selected deck and a
   * DIFFERENT agent on any other. M6's cross-deck switch is what consumes it.
   */
  | { kind: "agent"; deckId: string; agentId: string; from: "deck" | "overview" };

/**
 * An agent's tab membership exactly as the daemon reports it, mirroring
 * `DesktopAgentDto.tab`. It is the grouping key for the agent overview, which
 * would otherwise have to reconstruct membership from the role string —
 * everything below already reaches the webview and used to be discarded in
 * `agentFromDto`.
 */
export type AgentTab =
  | { kind: "dashboard" }
  | { kind: "mode"; name: string }
  /**
   * `cwd` is the ORCHESTRATION TAB's own directory, shared by every role pane
   * in the tab and distinct from each pane's `AgentSession.cwd` — an
   * orchestrator and its workers may sit in different per-pane directories
   * while belonging to one orchestration. Optional because the daemon reports
   * it only when the tab declared one (PRD #745 M8).
   */
  | { kind: "orchestration"; name: string; roleIndex: number; roleName: string; isStartRole: boolean; cwd?: string; displayTitle?: string; orchestrationId?: string };

/**
 * A pane the deck knows about.
 *
 * PRD #745 splits this interface in two, and the split is load-bearing rather
 * than cosmetic. The fields marked HONEST are ones the daemon genuinely
 * reports, so a screen may present them as fact. The fields marked
 * FIXTURE-ONLY have no source in daemon state at all — live mode hardcodes
 * them in `agentFromDto` to `"Unavailable"` / `0` / `"—"` — and they
 * exist solely because the existing control deck already renders them for the
 * deterministic fixture. **No new surface may read a FIXTURE-ONLY field**: a
 * design settled against one goes half-empty the moment it meets a real
 * daemon. See `OverviewAgent` in `components/AgentOverview.tsx`, which is the
 * compiler-enforced honest projection.
 */
export interface AgentSession {
  /** HONEST. Per-daemon monotonic integer, so it is unique only within a daemon. */
  id: string;
  /** HONEST. */
  paneId?: string;
  /** HONEST. Orchestration role name, else the agent type. */
  role: string;
  /** HONEST. */
  displayName: string;
  /**
   * HONEST. The BINARY the daemon says this agent runs — `claude`, `opencode`,
   * `codex` — as reported by the daemon that forked the process (issue #856).
   *
   * Optional, and absence renders as nothing. It is absent whenever the daemon
   * named no binary, which is the only honest answer available: nothing here
   * may fall back to this app's own copy of the agent registry, because that
   * copy is the divergence #856 closed. Same disposition as `lastActivityMs`
   * and `spawnedAtMs`.
   */
  cli?: string;
  /** FIXTURE-ONLY — the daemon tracks no model per agent (PRD #745, #633). */
  model: string;
  /** HONEST. */
  status: AgentStatus;
  /**
   * HONEST — the daemon's `lastUserPrompt`, else a restatement of `activeTool`,
   * else a placeholder saying the daemon reported neither.
   *
   * A DISPLAY COPY, sanitised and clamped to `DISPLAY_LIMITS.prompt` by
   * `agentFromDto`'s `taskLine`, because `AgentTile` prints it straight into a
   * DOM text node and the deck is the screen the app opens on. Bounding it at
   * the projection rather than at that one tile is deliberate: nothing sorts,
   * groups or keys on this field, so making it a display copy costs nothing and
   * makes every consumer of it safe by construction rather than by memory.
   * The raw prompt is on {@link AgentSession.lastUserPrompt} for surfaces that
   * want their own budget.
   */
  task: string;
  /**
   * HONEST, and ABSENT when the daemon reported none. NOT a worktree.
   *
   * Optional rather than sentinel-bearing since the M8 audit: it used to carry
   * {@link UNREPORTED}, which the daemon itself can legitimately report as a
   * directory name, so an agent could make its real working directory look
   * unreported by choosing the sentinel's spelling. Absence has no spelling and
   * so cannot be forged. Every surface decides for itself what absence looks
   * like — the overview renders nothing at all, the deck's tile prints
   * {@link UNREPORTED} at its own render seam.
   */
  cwd?: string;
  /**
   * FIXTURE-ONLY — no retry counter exists anywhere in the daemon, so live mode
   * reports NOTHING here rather than the `1` it used to hardcode: every tile
   * read `ATT 01` as if it were a fact (PRD #745 M8). Optional, so a surface
   * that renders it has to decide what absence looks like.
   */
  attempt?: number;
  /** FIXTURE-ONLY — `started_at` is invented on hydration, so a duration lies across a daemon restart. */
  duration: string;
  /** FIXTURE-ONLY — no token accounting in daemon state. */
  tokens: number;
  /** FIXTURE-ONLY — no cost accounting in daemon state. */
  cost: number;
  /** FIXTURE-ONLY — no context-window accounting in daemon state. */
  contextPercent: number;
  /** FIXTURE-ONLY — the daemon has no per-agent worktree or branch field. */
  worktree: string;
  /**
   * HONEST as of M8: live mode projects it from `SessionSnapshot.live_target`'s
   * `writable` half, which the desktop's own DTO used to drop.
   *
   * `"unknown"` is this field's sentinel for "the daemon declared no live
   * target". Unlike the `cwd` sentinel this one CANNOT collide with daemon
   * data — the desktop crate emits only the three mapped strings or omits the
   * key, so no daemon value spells `"unknown"` — and it is reversed to
   * `undefined` at the honest projection (`toOverviewAgent`) so no screen that
   * promises no placeholders can print it. Absence must NOT be read as
   * read-only: the TUI treats a missing `live_target` as the legacy live
   * default.
   */
  writeLease: "read" | "write" | "none" | "unknown";
  /**
   * HONEST. The most recent prompt the operator sent this agent
   * (`SessionSnapshot.last_user_prompt`), surfaced by M8 — the honest
   * replacement for live mode's hardcoded "Task metadata unavailable from
   * daemon". Optional rather than sentinel-bearing: a NEW field can represent
   * absence directly, so there is nothing here for a screen to leak.
   *
   * Free-form, agent-influenced text and the most attacker-shaped string the
   * overview renders, so every display copy goes through `displayText` with
   * `DISPLAY_LIMITS.prompt`.
   */
  lastUserPrompt?: string;
  /**
   * HONEST, and the only field on this interface the daemon did not already
   * send before PRD #745: M9 added `last_activity_ms` to `SessionSnapshot`.
   * Epoch milliseconds — when the daemon last saw this agent do anything.
   *
   * It is here where `duration` is FIXTURE-ONLY, and the difference is the
   * whole reason one shipped and the other did not. `started_at` is invented as
   * `now` on hydration, so a duration built from it resets and lies about
   * long-running work. `last_activity` is a high-water mark of real observed
   * event timestamps that the daemon never re-mints, and when the daemon cannot
   * vouch for it — it persists no session state, so a restart leaves it with
   * none — it is simply ABSENT and every surface renders nothing.
   *
   * Optional rather than sentinel-bearing, like `lastUserPrompt`. Rendering
   * goes through `displayActivity`, which owns the relative wording and refuses
   * to relativise an instant more than a minute in the future rather than
   * printing a negative "ago".
   */
  lastActivityMs?: number;
  /**
   * HONEST. When the daemon spawned this agent's process
   * (`AgentRecord.spawned_at_ms`, PRD #745 M11), as epoch milliseconds.
   *
   * This is the honest replacement for the FIXTURE-ONLY `duration` above, and
   * the reason one exists while the other never crossed the wire. The PRD
   * originally rejected a duration because `SessionState.started_at` is
   * invented as `now` on hydration; the deeper problem is that `started_at` is
   * EVENT-derived at all, so an agent that has never emitted a hook event has
   * no start instant — exactly the agent whose uptime a reader most wants. A
   * spawn is something the daemon DID, so it needs no signal and is never
   * inferred, and when the daemon cannot vouch for one it is simply ABSENT and
   * every surface renders nothing.
   *
   * A restarted worker gets a fresh record and so reads as its CURRENT
   * iteration; a role nobody restarted reads as its whole lifetime. Rendering
   * goes through `displayUptime`, which shares `displayActivity`'s clock-skew
   * rule and forks only the wording.
   */
  spawnedAtMs?: number;
  /** HONEST. */
  rows: number;
  /** HONEST. */
  cols: number;
  /** HONEST. Name of the tool the daemon last reported as active. */
  activeTool?: string;
  /** HONEST. The active tool's detail, when the daemon reported one. */
  activeToolDetail?: string;
  /** HONEST. */
  toolCount: number;
  /**
   * FIXTURE-ONLY, all five. These are deck-internal collections the fixture
   * populates to make the control deck's panels demonstrable; live mode leaves
   * every one of them empty (`agentFromDto`), so a surface that renders one
   * shows nothing at all against a real daemon.
   */
  transcript: string;
  diff: string[];
  checks: CheckResult[];
  handoffIds: string[];
  artifacts: Artifact[];
  /**
   * HONEST. Which daemon owns this agent. Agent ids are per-daemon monotonic
   * integers starting at 1, so two daemons both mint `"1"` — anything keyed by
   * a bare `id` is wrong the moment #742 connects a second daemon. New surfaces
   * key by the composite `(daemonId, id)`; the pre-existing bare-id maps in the
   * bridge, the deck and the terminal registry are #742's to fix.
   */
  daemonId: string;
  /** HONEST. Tab membership as the daemon reported it. Drives grouping. */
  tab: AgentTab;
  /** HONEST. True when the daemon reports this pane as an orchestration role pane. */
  inOrchestration?: boolean;
  /** HONEST. True for the orchestration's start role — the coordinator an operator should message. */
  isStartRole?: boolean;
}

export interface EvidenceItem {
  id: string;
  verdict: Verdict;
  title: string;
  summary: string;
  from: string;
  to: string;
  at: string;
  command?: string;
  exitCode?: number;
  reason: string;
  acknowledged: boolean;
  /** Deck-side agent this item is attributed to, when the daemon reported one. */
  agentId?: string;
}

export type Provider = "OpenAI" | "Anthropic" | "OpenCode" | "Custom";
export type PermissionMode = "default" | "read-only" | "workspace-write" | "full-access";
export type ProfileCommandMode = "generated" | "custom";

export interface AgentProfile {
  id: string;
  roleId: string;
  role: string;
  provider: Provider;
  cli: string;
  model: string;
  effort: "low" | "medium" | "high" | "xhigh";
  commandMode: ProfileCommandMode;
  command: string;
  customCommand?: string;
  permissionMode: PermissionMode;
  enabled: boolean;
  savedToProject: boolean;
}

export interface DeckSnapshot {
  runId: string;
  repo: string;
  /**
   * FIXTURE-ONLY. Nothing tracks a per-agent or per-run git branch daemon-side
   * — the only `git branch` calls in `src/` are deletions in the dispatch flows
   * — so live mode reports nothing here instead of the literal `"Unavailable"`
   * it used to put in the topbar (PRD #745 M8). Reconstructing it would mean a
   * subprocess per agent cwd on the daemon or a desktop-side git call that
   * breaks the local-daemons-only boundary; both are out of scope.
   */
  branch?: string;
  worktree: string;
  connection: ConnectionView;
  health: RunHealth;
  elapsed: string;
  spend: number;
  currentNode: number;
  totalNodes: number;
  /** FIXTURE-ONLY — see `AgentSession.attempt`. Absent in live mode. */
  currentAttempt?: number;
  paused: boolean;
  /**
   * The daemon's registered-schedule revision (issue #887) — carried solely so
   * `projectsRevision` can key on it, and rendered by nothing.
   *
   * Absent in fixture mode and from a daemon that reports none. See
   * `DesktopSnapshotDto.scheduleRevision` for what the number is and how it may
   * be compared.
   */
  scheduleRevision?: number;
  stages: WorkflowStage[];
  agents: AgentSession[];
  evidence: EvidenceItem[];
  /** Live delegation edges (handoff-visibility PRD D2), newest first. */
  handoffs: HandoffEdge[];
  profiles: AgentProfile[];
}

/**
 * Every deck the app is observing right now, one snapshot each (PRD #742 M4).
 *
 * # Selected deck first, and never empty
 *
 * The desktop crate's observed set is `[resolve().endpoint]` for every
 * selection except `All`, and for `All` it LEADS with the local deck — which is
 * what `All` resolves to. So the first entry is always the deck the
 * single-deck surfaces talk to, and there is always at least one: a deck the
 * app cannot reach is still an entry, carrying a `disconnected` connection and
 * no agents. That is the distinction the whole fleet view rests on — "no
 * agents" and "we cannot see the agents" are different statements, and an
 * absent entry could not tell them apart.
 *
 * # Keyed by `connection.deckId`
 *
 * That token is the crate's `EndpointIdentity::wire_id()`, and it is the same
 * value `daemonId` is derived from — so an entry here and the agents inside it
 * agree on identity by construction rather than by care.
 *
 * **It was `connection.socketPath` until PRD #742 M5**, which is `describe()` —
 * a label that renders neither the remote socket path, the identity file nor
 * the jump host. Two decks differing only in one of those folded into ONE entry
 * here and their agents shared a `daemonId`; the composite `(daemonId,
 * agentId)` key could not separate them, because the key component was the
 * collision.
 *
 * # Membership is exact, and comes from the wire
 *
 * `DesktopSnapshotDto.fleet` lists the observed decks on every snapshot, so a
 * deck that LEAVES the observed set is dropped on the next arrival from any
 * deck. M4 could only approximate this by resetting at `connect()`, because
 * nothing on the stream said a deck had gone.
 */
export type DeckFleet = DeckSnapshot[];

/** One delegation's lifecycle, driven by the daemon's handoff events. */
export interface HandoffEdge {
  /** The daemon's delegation id (`dlg-<millis>-<seq>`). */
  id: string;
  toRole: string;
  orchestration?: string;
  taskPreview?: string;
  /**
   * dispatched → delivered → done is the healthy path; failed is terminal and
   * carries `reason`. `respawned` marks that the worker was restarted for this
   * delegation (expected for clear=true roles).
   */
  status: "dispatched" | "delivered" | "failed" | "done";
  respawned: boolean;
  reason?: string;
  /** Wall-clock of the newest event applied to this edge (HH:MM:SS). */
  at: string;
}

export type DeckAction =
  | { type: "pause_run" }
  | { type: "resume_run" }
  | { type: "approve_run" }
  | { type: "advance_fixture" }
  | { type: "start_daemon" }
  | { type: "stop_daemon"; force?: boolean }
  | { type: "restart_daemon" }
  | { type: "allow_build_mismatch" }
  | { type: "start_workflow"; name: string; cwd: string; taskPrompt: string; roles: WorkflowLaunchRole[]; rows: number; cols: number; configRevision?: string }
  /**
   * Start one plain agent on the deck `deckId` names (PRD #1223 M3) — the
   * wire `connection.deckId` of the target, captured once when the user picks
   * the deck. Required, and never defaulted to the selected deck: under All
   * Decks the selection resolves to the local deck (#1083), which is exactly
   * the wrong answer on the overview. A deck the app is not observing is
   * refused and nothing starts anywhere. The new agent's id comes back as
   * `DeckActionResult.agentId`.
   */
  | { type: "start_agent"; deckId: string; command?: string; cwd?: string; displayName?: string; rows?: number; cols?: number }
  | { type: "retry_stage"; stageId: string }
  | { type: "stop_agent"; agentId: string }
  | { type: "rename_agent"; agentId: string; displayName: string }
  | { type: "submit_text"; agentId: string; text: string };

/**
 * The daemon's honest delivery outcome for submitted input, mirroring the Rust
 * `SendResult` (kebab-case on the wire). Only `applied` and `queued` mean the
 * text reached the agent; every other value — including an unrecognised future
 * one decoded as `unknown` — is a non-delivery the UI must not dress up as
 * success.
 */
export type SendResult =
  | "applied"
  | "queued"
  | "stale"
  | "wrong-session"
  | "history-only"
  | "no-live-target"
  | "ambiguous"
  | "unknown";

/**
 * What `desktop_run_action` reports back. The Rust command returns `ok: false`
 * with a non-delivered `sendResult` instead of raising, so a caller that only
 * awaits the promise cannot tell delivery from silent loss — every consumer of
 * `submit_text` must read this.
 */
export interface DeckActionResult {
  ok: boolean;
  sendResult?: SendResult;
  message?: string;
  /**
   * The id of the agent the action acted on — for `start_agent`, the one the
   * target deck just minted (PRD #1223 M3). Unique only within that deck, so
   * it means nothing without the `deckId` the action was sent with: a
   * consumer keys it as `(deckId, agentId)`, never alone.
   */
  agentId?: string;
}

/** True only for the two outcomes that actually reached the agent. */
export function isDelivered(result: DeckActionResult): boolean {
  return result.ok && (result.sendResult === undefined || result.sendResult === "applied" || result.sendResult === "queued");
}

/** Operator-facing explanation of a non-delivered outcome. */
export function sendResultReason(result: SendResult | undefined): string {
  switch (result) {
    case "stale": return "the deck's view of that pane had already moved on";
    case "wrong-session": return "the pane handle no longer maps to that agent's session";
    case "history-only": return "the agent has no live pane — only its history remains";
    case "no-live-target": return "there is nothing live to write to";
    case "ambiguous": return "the write started but did not complete; some of it may already have landed, so it was not retried";
    case "unknown": return "the deck reported an outcome this build does not recognise";
    default: return "the deck did not confirm delivery";
  }
}

export interface WorkflowLaunchRole {
  role: string;
  command: string;
  start: boolean;
}

export interface WorkflowLaunchConfig {
  /** The daemon's own spelling of the orchestration name, submitted verbatim. */
  name: string;
  /**
   * The daemon-canonical project path, straight off the resolved selection.
   * PRD #819 M6: never typed into this form and never derived from the app's
   * own environment — the only two sources are a path the daemon listed and a
   * path the user pasted into the project picker, which the daemon then
   * resolved and re-spelled.
   */
  cwd: string;
  /**
   * The escaped twins of `name` and `cwd`, for the confirmation dialog and the
   * result notice (PRD #819 audit fix). Carried on the config so the screen
   * that renders them does no lookup of its own; both are destructured off
   * before the action is dispatched, the way `customCommandCount` already is,
   * so neither reaches the daemon.
   *
   * The builder falls back to the identity if it somehow has no orchestration
   * to take a label from, which cannot happen while the launch button is
   * enabled — `canLaunch` requires one — and is a worse-label fallback rather
   * than a blank one if it ever does.
   */
  displayName: string;
  displayPath: string;
  taskPrompt: string;
  roles: WorkflowLaunchRole[];
  rows: number;
  cols: number;
  customCommandCount: number;
  generatedFullAccessCount: number;
  /** The revision the selection resolved against, if the daemon reported one. */
  configRevision?: string;
}

export interface TerminalChunk {
  agentId: string;
  /**
   * PRD #1105 security audit — the deck whose daemon produced these bytes, as
   * the bridge knew it at delivery time.
   *
   * Stamped by the producer rather than inferred by the consumer, because the
   * bridge learns the selected deck off `DesktopSnapshotDto.fleet[0]`
   * synchronously while React state is a commit behind it. Optional so a
   * producer that cannot name a deck stays valid; the runtime then falls back
   * to the deck it currently believes is selected, which is what a bare-id
   * producer implicitly meant.
   */
  deckId?: string;
  data: Uint8Array;
  stream: "output" | "end" | "error";
  operation: "append" | "replace";
  generation?: number;
  message?: string;
}

export interface TerminalBuffer {
  data: Uint8Array;
  baseOffset: number;
  generation?: number;
}

/**
 * The direct PTY-byte path, addressed by the COMPOSITE `(deckId, agentId)`.
 *
 * PRD #1105's security audit is why the deck travels here. Buffers used to be
 * keyed by bare agent id, and nothing cleared them when the selected deck moved
 * — so mounting a viewport for deck B's `planner` read deck A's retained
 * buffer and wrote up to a megabyte of another machine's output into the new
 * xterm, under B's correctly resolved heading. See {@link agentKey}.
 */
export interface TerminalFeed {
  get(deckId: string | undefined, agentId: string): TerminalBuffer | undefined;
  subscribe(deckId: string | undefined, agentId: string, listener: (buffer: TerminalBuffer) => void): () => void;
}

/**
 * One agent on one deck — the whole identity of a terminal seam, passed as a
 * value rather than assembled from a bare id and whatever deck happens to be
 * selected when the call lands.
 *
 * # Why this is a parameter and not something the bridge can look up
 *
 * Every terminal verb used to take a bare `agentId` and resolve the daemon
 * through the process-global selected endpoint. Agent ids are per-daemon
 * monotonic integers, so `"planner"` names an agent on every deck: with the
 * agent pane able to attach a terminal on a deck that is NOT the selected one,
 * a bare id routes this client's keystrokes to whichever machine happens to be
 * selected at the instant the write lands. Issue
 * [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116) is two audit
 * rounds of that one shape — identity read from mutable current selection at
 * use time rather than captured at creation.
 *
 * # It is compared BY VALUE, never by reference
 *
 * Nothing may key a `Map` on a `AgentTarget` object. The same target is
 * re-allocated on every render — `{ deckId: agent.daemonId, agentId: agent.id }`
 * is a fresh object each time — so an identity-keyed lookup passes a test that
 * happens to reuse one object and fails in production. {@link agentKey} is the
 * one way to turn a target into a key.
 */
export interface AgentTarget {
  deckId: string;
  agentId: string;
}

export interface DeckRuntimeState {
  mode: RuntimeMode;
  /**
   * The deck every SINGLE-DECK surface is bound to — the deck screen, its
   * terminals, and every action. Identical to `fleet[0]` (PRD #742 M4), and
   * kept as its own member because "the selected deck" is what these screens
   * mean and reading it as an index would put the invariant at each call site.
   */
  snapshot: DeckSnapshot;
  /**
   * Every deck the app is observing, selected first (PRD #742 M4). One entry
   * under every selection but `All`, where it is the whole configured fleet.
   *
   * The agent overview renders one group per entry. Nothing else does: a tile's
   * terminal is always the selected deck's, because an attach costs one
   * connection and one daemon-side task PER VISIBLE TILE and attach streams
   * carry no stream id, so N decks of rows is cheap and N decks of live
   * terminals is not (PRD #742 DECISION 1).
   */
  fleet: DeckFleet;
  terminalData: Record<string, TerminalBuffer>;
  /** Direct PTY-byte path that bypasses React state; absent in tests/fixture. */
  terminalFeed?: TerminalFeed;
  error?: string;
  /**
   * Drop the last action's error (issue #1046).
   *
   * Required rather than optional: the toast in `App.tsx` renders on
   * `notice || error`, so a runtime that cannot clear `error` produces a toast
   * whose dismiss button silently does nothing — which is exactly the bug this
   * closes, and a fake that omits this member should stop type-checking.
   */
  clearError: () => void;
  runAction: (action: DeckAction) => Promise<DeckActionResult>;
  /**
   * Issue #1042 — the last NON-DELIVERED `SendResult` the guarded send verb
   * returned, per agent id. An agent with no entry has nothing unresolved.
   *
   * This is the post-hoc half of the terminal's input state. `history-only` and
   * `no-live-target` are read off `AgentSession.writeLease` and need no send;
   * `wrong-session` is decided at write time, is carried by no snapshot field,
   * and after a rollover the pane reads `writeLease === "write"` — it looks
   * deliverable precisely when a send would fail. So it can only be known by
   * trying, which makes a returned verdict the only place it can come from.
   *
   * Optional, and absence is a real state rather than an oversight: a runtime
   * through which nothing has ever been submitted has no verdicts to report.
   *
   * **Keyed by `agentKey(deckId, agentId)` since PRD #1105's security audit**,
   * not by the bare id. A verdict recorded for deck A's `planner` would
   * otherwise disable deck B's pane and render A's rejection notice under B's
   * heading — the ids collide across decks by construction.
   */
  terminalInputResults?: Record<string, SendResult>;
  sendTerminalInput: (target: AgentTarget, data: string) => Promise<void>;
  resizeTerminal: (target: AgentTarget, cols: number, rows: number) => Promise<void>;
  /**
   * PRD #882 — the geometry the daemon has APPLIED per agent, keyed by
   * `agentKey(deckId, agentId)`.
   *
   * **The deck is part of that key since PRD #1105's security audit.** Nothing
   * evicts an entry per agent, so a pane opened for deck B's `planner` applied
   * deck A's grid, and an attach that won the race against the pane's first fit
   * submitted A's cached dimensions to B — transiently reflowing B's PTY, and
   * every other viewer attached to it, from a viewport on another machine.
   *
   * A terminal must render at this, not at the size of its own tile: a PTY has
   * one window size, so the daemon sizes each agent to the smallest pane among
   * every client attached and larger panes pad the remainder.
   *
   * Optional, and absence is a real state rather than an oversight — the browser
   * preview has no daemon, and a live runtime has nothing here until the first
   * attach answers. A tile with no entry keeps whatever its own fit produced,
   * which is the correct answer when nothing is disagreeing with it.
   */
  appliedGeometry?: Record<string, { rows: number; cols: number }>;
  /**
   * States the whole set of agents whose terminal is on screen (PRD #745 M7).
   * A screen that mounts terminals calls this once per render commit with every
   * shown target; a screen that mounts none calls it with `[]`. Attach follows
   * this and nothing else, so a screen that renders no output opens no PTYs
   * either.
   *
   * A target rather than a bare id since PRD #1105's cross-deck pane: the
   * overview can show one deck's agents while a pane holds a terminal on
   * another, and both declarations travel in the same array.
   */
  setShownTerminals: (targets: AgentTarget[]) => Promise<void>;
  reconnect: () => Promise<void>;
  /**
   * PRD #819 M6: the projects the connected daemon knows about. There is no
   * client-side list to fall back to — an unreachable daemon means no projects
   * to offer, which is an honest answer and not a reason to guess one.
   */
  listProjects: () => Promise<DaemonProjectListing>;
  /**
   * Resolve ONE path — one the daemon listed, or one the user pasted. The reply
   * carries the daemon's canonical spelling, which is the string every later
   * request uses.
   */
  resolveProject: (path: string) => Promise<DaemonResolvedProject>;
  /** The desktop app's own settings, and where they live (PRD #803). */
  getSettings: () => Promise<import("./lib/bridge").DesktopSettingsSnapshotDto>;
  /** Persist the whole document; resolves to what was written. */
  saveSettings: (settings: import("./lib/bridge").DesktopSettingsDto) => Promise<import("./lib/bridge").DesktopSettingsDto>;
  /**
   * Test one deck end to end and resolve with a named state (PRD #741 M10).
   *
   * Rejects only when the call itself could not be made: a deck that failed is
   * a report, not an exception, because "your ssh config has never seen this
   * host's key" and "the deck over there is not running" are different things
   * for a user to do next.
   */
  testEndpoint: (
    settings: import("./lib/bridge").DesktopSettingsDto,
    selection: string,
  ) => Promise<import("./lib/bridge").EndpointTestReportDto>;
  /**
   * Whether a credential is stored, without reading it (PRD #802 M4).
   *
   * There is deliberately no counterpart that READS one: PRD #803's rule is
   * that a secret goes in neither `desktop.toml` nor `localStorage`, and a
   * value reaching this side is one `JSON.stringify` from the second half of
   * that. The backends that need the value make their call Rust-side, which is
   * where the CSP already forces every network hop.
   *
   * Never rejects for "I could not find out" — that arrives as `problem`,
   * which is a different answer from "nothing is stored".
   */
  secretStatus: (
    id: import("./lib/bridge").VoiceSecretId,
  ) => Promise<import("./lib/bridge").SecretStatusDto>;
  /**
   * Replace a stored credential, resolving with the new status.
   *
   * **Rejects when the store failed**, so a failure can never render as a
   * saved key — which is the outcome PRD #802 M4 is written against.
   */
  storeSecret: (
    id: import("./lib/bridge").VoiceSecretId,
    secret: string,
  ) => Promise<import("./lib/bridge").SecretStatusDto>;
  /** Forget a stored credential. Rejects when the store failed. */
  forgetSecret: (
    id: import("./lib/bridge").VoiceSecretId,
  ) => Promise<import("./lib/bridge").SecretStatusDto>;
  /**
   * Voice control (PRD #802 M6): resolve one utterance into an outcome carrying
   * the sentence to show, and drive the microphone that can produce one.
   *
   * **All six are optional, and absence is a real state rather than an
   * oversight.** A runtime with no `resolveVoice` cannot run a voice command at
   * all, so the surface offers no Voice control for it — the same reasoning the
   * microphone gets one layer down, where `available: false` renders no mic
   * rather than a broken-looking one. A control that opened onto a panel with
   * nothing behind it would be worse than its absence, and several of this app's
   * render-only test runtimes are exactly that shape.
   *
   * `declareVoiceScreen` states which screen a command would run against. It is
   * a declaration rather than a parameter of `resolveVoice` because the mounted
   * screen is the one piece of live state that exists ONLY in the webview; see
   * `DeckBridge.declareVoiceScreen` for the whole of that seam.
   */
  declareVoiceScreen?: (screen: import("./lib/bridge").VoiceScreen) => void;
  resolveVoice?: (utterance: string) => Promise<import("./lib/bridge").VoiceResultDto>;
  /**
   * Every command in the table, annotated for one screen (PRD #802 D7) — what
   * the discovery overlay lists.
   *
   * Optional alongside the rest of the voice members and for the same reason:
   * a runtime that cannot resolve an utterance has no vocabulary worth showing,
   * and several render-only test runtimes are exactly that shape. A dispatch of
   * `list_commands` against such a runtime is refused before it runs, and the
   * surface says the command is not wired to anything in this build — which is
   * literally what is true.
   */
  voiceCommands?: (
    screen: import("./lib/bridge").VoiceScreen,
  ) => Promise<import("./lib/bridge").VoiceCommandDto[]>;
  /** Open the microphone. Rejects with the not-configured sentence when transcription is off. */
  voiceStart?: () => Promise<import("./lib/bridge").VoiceStatusDto>;
  /** Close the microphone and transcribe what it heard. No audio crosses the boundary. */
  voiceStop?: () => Promise<import("./lib/bridge").VoiceTranscriptionDto>;
  /**
   * What the microphone is doing, and whether one is offered at all.
   *
   * Polled between a start and a stop, because it is the only way the surface
   * learns the length cap ended the recording on its own.
   */
  voiceStatus?: () => Promise<import("./lib/bridge").VoiceStatusDto>;
  /** Abandon a recording without transcribing it. Idempotent and never refused. */
  voiceCancel?: () => Promise<import("./lib/bridge").VoiceStatusDto>;
  /**
   * Scale the whole window, terminals included (PRD #744).
   *
   * Applying only — the level is persisted through `saveSettings`, behind a
   * coalescer, because a held zoom key would otherwise rewrite `desktop.toml`
   * once per key repeat. Resolves to the level actually applied, which is the
   * caller's level snapped to the ladder.
   *
   * A no-op outside the Tauri window: a browser has its own zoom, and there is
   * no webview to scale.
   */
  setZoom: (level: number) => Promise<number>;
}
