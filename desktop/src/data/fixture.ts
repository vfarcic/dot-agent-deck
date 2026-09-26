import type { VoiceCommandDto, VoiceResolvedParamDto, VoiceResultDto, VoiceScreen, VoiceStatusDto, VoiceTranscriptionDto } from "../lib/bridge";
import type { AgentProfile, AgentSession, AgentStatus, AgentTab, DaemonOrchestration, DeckDirectoryEntry, DeckSnapshot, EvidenceItem, NewAgentOption, WorkflowStage } from "../types";

/**
 * The fixture's stand-in for a deck identity — used as BOTH `deckId` and
 * `socketPath`, which is the one place the fixture deliberately differs from
 * live mode.
 *
 * Live mode's two values are different things since PRD #742 M5: `deckId` is an
 * opaque `deck-<16 hex>` token minted from `EndpointIdentity`, and `socketPath`
 * is the `Endpoint::describe()` label beside it. The fixture keeps one readable
 * string in both because every deck here is distinct by its label anyway, and a
 * fake hash would make each of these decks harder to recognise in a DOM dump for
 * no property gained. The collision case the id exists for — two decks the label
 * cannot tell apart — is exercised where it belongs, against the real mapping,
 * in `bridge.test.ts`.
 */
export const FIXTURE_DAEMON_ID = "/tmp/dot-agent-deck.sock";

/**
 * The remote decks the `fleet` scenario adds (PRD #742 M4), named the way the
 * desktop crate names a remote one: `Endpoint::describe()` renders a remote
 * deck as `user@host[:port]`, never as a socket path, so the fixture's ids are
 * the shape a real fleet's are.
 */
export const FIXTURE_REMOTE_DAEMON_ID = "dev@build-box";
export const FIXTURE_UNREACHABLE_DAEMON_ID = "ci@runner-7";

/**
 * The deck that is in the fleet and has not reported yet (PRD #742 M14).
 *
 * A remote deck, because that is the only kind the state lasts long enough to
 * see: a local deck is resolved by the bootstrap itself, while a remote one is
 * a tunnel, a handshake and a `ListAgents` away — bounded by the crate's
 * reconcile interval for a quiet deck and by `FORWARD_READY_TIMEOUT` (30s) for
 * one whose tunnel never comes up.
 *
 * It stays pending forever in the fixture, which a live deck does not. That is
 * the same licence the unreachable deck above takes: a fixture is one instant
 * held still, and holding still is exactly what lets this tier assert that the
 * instant reads as a deck on its way rather than as a deck that failed.
 */
export const FIXTURE_PENDING_DAEMON_ID = "ops@edge-3";

/** Which scenario `createFixtureFleet` builds; selected by `?state=`. */
export type FixtureState = "connected" | "disconnected" | "error" | "empty" | "crowded" | "fleet";

export const DEFAULT_PROFILES: AgentProfile[] = [
  {
    id: "orchestrator",
    roleId: "orchestrator",
    role: "Orchestrator",
    provider: "Anthropic",
    cli: "claude",
    model: "claude-opus-5",
    effort: "high",
    commandMode: "generated",
    command: "claude --model opus --effort high --permission-mode default",
    permissionMode: "default",
    enabled: true,
    savedToProject: true,
  },
  {
    id: "coder",
    roleId: "coder",
    role: "Coder",
    provider: "OpenAI",
    cli: "codex",
    model: "gpt-5.6-sol",
    effort: "medium",
    commandMode: "generated",
    command: "codex --model gpt-5.6-sol --sandbox workspace-write --ask-for-approval on-request -c model_reasoning_effort=medium",
    permissionMode: "workspace-write",
    enabled: true,
    savedToProject: true,
  },
  {
    id: "reviewer",
    roleId: "reviewer",
    role: "Reviewer",
    provider: "OpenAI",
    cli: "codex",
    model: "gpt-5.6-sol",
    effort: "high",
    commandMode: "generated",
    command: "codex --model gpt-5.6-sol --sandbox read-only --ask-for-approval on-request -c model_reasoning_effort=high",
    permissionMode: "read-only",
    enabled: true,
    savedToProject: true,
  },
  {
    id: "auditor",
    roleId: "auditor",
    role: "Auditor",
    provider: "OpenAI",
    cli: "codex",
    model: "gpt-5.6-sol",
    effort: "xhigh",
    commandMode: "generated",
    command: "codex --model gpt-5.6-sol --sandbox read-only --ask-for-approval on-request -c model_reasoning_effort=xhigh",
    permissionMode: "read-only",
    enabled: true,
    savedToProject: true,
  },
  {
    id: "tester",
    roleId: "tester",
    role: "Tester",
    provider: "OpenAI",
    cli: "codex",
    model: "gpt-5.6-sol",
    effort: "medium",
    commandMode: "generated",
    command: "codex --model gpt-5.6-sol --sandbox workspace-write --ask-for-approval on-request -c model_reasoning_effort=medium",
    permissionMode: "workspace-write",
    enabled: true,
    savedToProject: true,
  },
  {
    id: "release",
    roleId: "release",
    role: "Release",
    provider: "Anthropic",
    cli: "claude",
    model: "claude-sonnet-5",
    effort: "medium",
    commandMode: "generated",
    command: "claude --model sonnet --effort medium --permission-mode default",
    permissionMode: "default",
    enabled: true,
    savedToProject: true,
  },
];

const stages: WorkflowStage[] = [
  { id: "plan", label: "Plan", agentId: "planner", status: "passed", attempt: 1, enabled: true },
  { id: "build", label: "Build", agentId: "builder", status: "passed", attempt: 2, enabled: true },
  { id: "validate", label: "Validate", agentId: "builder", status: "passed", attempt: 2, enabled: true },
  { id: "review", label: "Review", agentId: "reviewer", status: "active", attempt: 1, enabled: true },
  { id: "test", label: "Test", agentId: "tester", status: "queued", attempt: 1, enabled: true },
  { id: "approve", label: "Human", status: "waiting", attempt: 1, enabled: true },
];

const evidence: EvidenceItem[] = [
  {
    id: "ev-review",
    verdict: "INFO",
    title: "Review in progress",
    summary: "Reviewer is tracing the terminal bridge and checking the retry patch against the acceptance contract.",
    from: "Builder",
    to: "Reviewer",
    at: "14:42:18",
    reason: "Build and validation passed after the second attempt, so the review edge opened.",
    acknowledged: true,
  },
  {
    id: "ev-pass",
    verdict: "PASS",
    title: "Validation recovered",
    summary: "The focused test and the complete frontend suite pass after the stale subscription cleanup was fixed.",
    from: "Validation",
    to: "Reviewer",
    at: "14:41:52",
    command: "pnpm test --run",
    exitCode: 0,
    reason: "Required checks are green and the retry budget remains within policy.",
    acknowledged: true,
  },
  {
    id: "ev-fix",
    verdict: "FIX",
    title: "Fixture exposed stale listener",
    summary: "The first build retained a Tauri listener after terminal detach and duplicated output on reconnect.",
    from: "Validation",
    to: "Builder",
    at: "14:37:09",
    command: "pnpm test -- bridge",
    exitCode: 1,
    reason: "A failed transition returns to the owning role with precise evidence, not a pasted transcript.",
    acknowledged: true,
  },
  {
    id: "ev-plan",
    verdict: "PASS",
    title: "Implementation contract accepted",
    summary: "The plan limits this milestone to the daemon client, terminal grid, agent profiles, and observable loop.",
    from: "Planner",
    to: "Builder",
    at: "14:32:44",
    reason: "Scope, file ownership, and exit criteria were explicit before write access transferred.",
    acknowledged: true,
  },
];

const agents: AgentSession[] = [
  {
    id: "planner",
    daemonId: FIXTURE_DAEMON_ID,
    tab: { kind: "dashboard" },
    role: "Planner",
    displayName: "Plan / architecture",
    cli: "claude",
    model: "opus-5",
    status: "passed",
    task: "Define the smallest observable coding loop and its acceptance contract.",
    cwd: "/dev/active/dot-agent-deck-gui",
    attempt: 1,
    duration: "04:18",
    tokens: 18240,
    cost: 0.82,
    contextPercent: 38,
    worktree: "codex/visual-control-deck",
    writeLease: "read",
    rows: 32,
    cols: 110,
    toolCount: 6,
    transcript: "\u001b[2m14:28:32\u001b[0m  reading repository contract\r\n\u001b[2m14:29:16\u001b[0m  mapping daemon/client seams\r\n\u001b[32mPASS\u001b[0m  plan accepted · 6 exit criteria · 0 unresolved decisions\r\n\r\nHandoff → Builder\r\n  Implement the Tauri client as a second daemon surface.\r\n  Keep terminal bytes out of structured event state.\r\n",
    diff: [],
    checks: [{ id: "plan-contract", name: "Acceptance contract", status: "passed", duration: "0.2s" }],
    handoffIds: ["ev-plan"],
    artifacts: [{ id: "prd", name: "Desktop GUI PRD", kind: "file", path: "prds/done/176-desktop-gui.md" }],
  },
  {
    id: "builder",
    daemonId: FIXTURE_DAEMON_ID,
    tab: { kind: "dashboard" },
    role: "Builder",
    displayName: "Desktop implementation",
    cli: "codex",
    model: "gpt-5.6-sol",
    status: "passed",
    task: "Build the visual control room and bridge it to the existing daemon.",
    cwd: "/dev/active/dot-agent-deck-gui",
    attempt: 2,
    duration: "09:44",
    tokens: 28612,
    cost: 1.34,
    contextPercent: 52,
    worktree: "codex/visual-control-deck",
    writeLease: "write",
    rows: 32,
    cols: 110,
    activeTool: "cargo check",
    toolCount: 18,
    transcript: "\u001b[2m14:34:02\u001b[0m  added desktop workspace scaffold\r\n\u001b[2m14:36:41\u001b[0m  wired daemon snapshot + terminal events\r\n\u001b[31mFAIL\u001b[0m  bridge test: listener disposed twice\r\n\u001b[33mRETRY 2/3\u001b[0m  isolating failed subscription case\r\n\u001b[2m14:40:58\u001b[0m  fixed idempotent detach cleanup\r\n\u001b[32mPASS\u001b[0m  24 tests · 0 warnings\r\n\r\nWaiting for reviewer evidence…\r\n",
    diff: ["+ desktop/src/App.tsx", "+ desktop/src/lib/bridge.ts", "+ desktop/src/styles.css", "~ Cargo.toml"],
    checks: [
      { id: "typecheck", name: "TypeScript", status: "passed", duration: "2.1s", command: "pnpm tsc" },
      { id: "unit", name: "Unit tests", status: "passed", duration: "4.8s", command: "pnpm test --run" },
      { id: "rust", name: "Rust check", status: "passed", duration: "8.4s", command: "cargo check" },
    ],
    handoffIds: ["ev-fix", "ev-pass"],
    artifacts: [{ id: "cast", name: "Control room smoke run", kind: "recording", path: ".dot-agent-deck/recordings/gui.cast" }],
  },
  {
    id: "reviewer",
    daemonId: FIXTURE_DAEMON_ID,
    tab: { kind: "dashboard" },
    role: "Reviewer",
    displayName: "Contract review",
    cli: "codex",
    model: "gpt-5.6-sol",
    status: "running",
    task: "Audit terminal lifecycle, unsafe actions, and evidence integrity.",
    cwd: "/dev/active/dot-agent-deck-gui",
    attempt: 1,
    duration: "02:26",
    tokens: 9568,
    cost: 0.41,
    contextPercent: 21,
    worktree: "codex/visual-control-deck",
    writeLease: "read",
    rows: 32,
    cols: 110,
    activeTool: "rg",
    toolCount: 8,
    transcript: "\u001b[2m14:42:02\u001b[0m  reviewing frontend bridge DTOs\r\n\u001b[2m14:42:13\u001b[0m  checking destructive action gates\r\n\u001b[36mACTIVE\u001b[0m  tracing terminal detach → listener cleanup\r\n\r\n$ rg \"desktop_terminal\" desktop/src desktop/src-tauri\r\n",
    diff: [],
    checks: [{ id: "review", name: "Interface review", status: "running", command: "review contract" }],
    handoffIds: ["ev-review"],
    artifacts: [],
  },
  {
    id: "tester",
    daemonId: FIXTURE_DAEMON_ID,
    tab: { kind: "dashboard" },
    role: "Tester",
    displayName: "User-path verification",
    cli: "codex",
    model: "gpt-5.6-sol",
    status: "queued",
    task: "Exercise fixture, live disconnect, terminal input, and approval paths.",
    cwd: "/dev/active/dot-agent-deck-gui",
    attempt: 1,
    duration: "00:00",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: "codex/visual-control-deck",
    writeLease: "none",
    rows: 32,
    cols: 110,
    toolCount: 0,
    transcript: "\u001b[2mQueued\u001b[0m\r\n\r\nStarts after Reviewer returns PASS.\r\n",
    diff: [],
    checks: [
      { id: "browser", name: "Browser smoke", status: "queued" },
      { id: "a11y", name: "Accessibility", status: "queued" },
      { id: "pty", name: "Real PTY path", status: "queued" },
    ],
    handoffIds: [],
    artifacts: [],
  },
];

/** One agent in the crowded scenario, described only by what a daemon reports. */
interface CrowdedSeed {
  id: string;
  displayName: string;
  role: string;
  /** The daemon's own agent-type vocabulary, exactly as live mode renders it. */
  cli: string;
  status: AgentStatus;
  cwd: string;
  tab: AgentTab;
  activeTool?: string;
  activeToolDetail?: string;
  toolCount: number;
  /**
   * `SessionSnapshot.last_user_prompt`, which live mode reports as of M8.
   * Deliberately absent on some seeds: an agent that has emitted no prompt
   * event is the ordinary case, and the screen has to look right for it.
   */
  lastUserPrompt?: string;
  /**
   * `SessionSnapshot.live_target`, likewise reported as of M8. Omitted where
   * the daemon would declare no live target — which the fixture keeps as
   * `"unknown"`, the same sentinel `agentFromDto` writes.
   */
  writeLease?: AgentSession["writeLease"];
  /**
   * `SessionSnapshot.last_activity_ms`, reported as of M9. Given as an AGE in
   * minutes rather than an absolute instant so a fixture capture reads the same
   * on any day, and left off some seeds because a daemon with no live session
   * for an agent reports none — the blank case the screen has to look right for.
   */
  quietForMinutes?: number;
  /**
   * `AgentRecord.spawned_at_ms`, reported as of M11. Given as an AGE in minutes
   * for the same reason `quietForMinutes` is, and left off some seeds because a
   * daemon that did not spawn an agent reports no spawn time — the blank case
   * the screen has to look right for.
   *
   * Every seed that has one has it at or above its `quietForMinutes`: an agent
   * cannot have last done something before it existed, and a fixture that
   * showed otherwise would be previewing a state no daemon can produce.
   */
  upForMinutes?: number;
}

/**
 * Builds a crowded-scenario agent whose every non-daemon-reported field carries
 * the SAME placeholder `agentFromDto` hardcodes in live mode, rather than a
 * plausible-looking number. That makes `?fixture=1&state=crowded` a faithful
 * preview of a real daemon at fifteen agents on both screens, instead of a demo
 * that flatters a design with data the daemon cannot supply (PRD #745 M1).
 */
function crowdedAgent(seed: CrowdedSeed): AgentSession {
  const orchestration = seed.tab.kind === "orchestration" ? seed.tab : undefined;
  return {
    id: seed.id,
    daemonId: FIXTURE_DAEMON_ID,
    role: seed.role,
    displayName: seed.displayName,
    cli: seed.cli,
    model: "Unavailable",
    status: seed.status,
    task: seed.lastUserPrompt
      ?? (seed.activeTool
        ? `Active tool: ${seed.activeTool}${seed.activeToolDetail ? ` · ${seed.activeToolDetail}` : ""}`
        : "Task metadata unavailable from the daemon"),
    cwd: seed.cwd,
    // No attempt: live mode reports none, and the crowded scenario is meant to
    // be a faithful preview of a real daemon (PRD #745 M8).
    duration: "—",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: "Unavailable",
    writeLease: seed.writeLease ?? "unknown",
    lastUserPrompt: seed.lastUserPrompt,
    // Relative to the moment the fixture is built, so `?fixture=1&state=crowded`
    // shows a spread of ages — "just now" through days — instead of one frozen
    // instant that reads as a fleet nobody has touched since the fixture was
    // written.
    lastActivityMs: seed.quietForMinutes === undefined ? undefined : Date.now() - seed.quietForMinutes * 60_000,
    // Likewise relative, so the uptime column shows a spread — minutes through
    // days — rather than one frozen instant (PRD #745 M11).
    spawnedAtMs: seed.upForMinutes === undefined ? undefined : Date.now() - seed.upForMinutes * 60_000,
    rows: 40,
    cols: 132,
    activeTool: seed.activeTool,
    activeToolDetail: seed.activeToolDetail,
    toolCount: seed.toolCount,
    transcript: "",
    diff: [],
    checks: [],
    handoffIds: [],
    artifacts: [],
    tab: seed.tab,
    inOrchestration: Boolean(orchestration),
    isStartRole: orchestration?.isStartRole ?? false,
  };
}

/** What the preview was asked to start — `start_agent`'s fields plus the id the fixture deck minted. */
export interface FixtureStartedAgent {
  id: string;
  daemonId: string;
  displayName?: string;
  command?: string;
  cwd?: string;
  rows?: number;
  cols?: number;
}

/**
 * The agent a fixture deck gains when the preview starts one (PRD #1223 M3).
 *
 * Shaped the way `agentFromDto` shapes a freshly spawned LIVE agent, by the
 * convention {@link crowdedAgent} set: an agent that has emitted no hook event
 * yet is `running` (the crate maps a record with no hook state to `running`),
 * every field the daemon does not report carries live mode's placeholder, and
 * there is no tool, prompt or activity to show. The CLI is the command's first
 * word and absent when no command was given — the daemon then starts its
 * default shell and names no binary, and absence is the honest rendering.
 */
export function createFixtureStartedAgent(started: FixtureStartedAgent): AgentSession {
  const cli = started.command?.trim().split(/\s+/)[0] || undefined;
  const role = cli ? cli.charAt(0).toUpperCase() + cli.slice(1) : "Agent";
  return {
    id: started.id,
    daemonId: started.daemonId,
    role,
    displayName: started.displayName || role,
    cli,
    model: "Unavailable",
    status: "running",
    task: "Task metadata unavailable from the daemon",
    cwd: started.cwd,
    duration: "—",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: "Unavailable",
    writeLease: "unknown",
    spawnedAtMs: Date.now(),
    rows: started.rows ?? 24,
    cols: started.cols ?? 80,
    toolCount: 0,
    transcript: "",
    diff: [],
    checks: [],
    handoffIds: [],
    artifacts: [],
    tab: { kind: "dashboard" },
    inOrchestration: false,
    isStartRole: false,
  };
}

/**
 * The next id a fixture deck mints: the lowest positive integer none of its
 * agents already uses. A daemon mints per-daemon monotonic integers, so two
 * decks answering the same id is the ordinary case the preview must show too.
 */
export function nextFixtureAgentId(agents: readonly AgentSession[]): string {
  let next = 1;
  while (agents.some((agent) => agent.id === String(next))) next += 1;
  return String(next);
}

/**
 * PRD #1223 M4 — each connected fixture deck's home directory, which is where
 * the New agent dialog's directory step opens on it.
 *
 * Different per deck on purpose: a preview that listed the wrong deck's tree
 * would show the wrong home, where two identical trees would hide it.
 */
export const FIXTURE_HOMES: Readonly<Record<string, string>> = {
  [FIXTURE_DAEMON_ID]: "/home/dev",
  [FIXTURE_REMOTE_DAEMON_ID]: "/home/build",
};

/** One directory of a fixture deck's tree, shaped the way a deck lists one. */
export interface FixtureDirectory {
  path: string;
  parent?: string;
  entries: DeckDirectoryEntry[];
}

/**
 * The filesystem a fixture deck lists, rooted at `/`: a home holding a project
 * directory (it carries the marker) and an ordinary one, and a directory one
 * level deeper inside the ordinary one — enough to browse into, confirm a
 * directory with no subdirectories, and go back up through every parent.
 *
 * Every path here is the fixture DECK's answer, the way a daemon answers with
 * its own canonical spelling. The dialog never builds one of these itself.
 */
export function fixtureDirectoryTree(home: string): Map<string, FixtureDirectory> {
  const user = home.split("/").filter(Boolean).at(-1) ?? "dev";
  const entry = (path: string, isProject = false): DeckDirectoryEntry => ({ path, displayName: path.split("/").at(-1) ?? path, isProject });
  const tree: FixtureDirectory[] = [
    { path: "/", entries: [entry("/home")] },
    { path: "/home", parent: "/", entries: [entry(`/home/${user}`)] },
    { path: home, parent: "/home", entries: [entry(`${home}/demo-project`, true), entry(`${home}/scratch`)] },
    { path: `${home}/demo-project`, parent: home, entries: [] },
    { path: `${home}/scratch`, parent: home, entries: [entry(`${home}/scratch/notes`), entry(`${home}/scratch/twin-project`, true)] },
    { path: `${home}/scratch/notes`, parent: `${home}/scratch`, entries: [] },
    { path: `${home}/scratch/twin-project`, parent: `${home}/scratch`, entries: [] },
  ];
  return new Map(tree.map((directory) => [directory.path, directory]));
}

/**
 * PRD #1223 M6 — the orchestrations a fixture deck's `demo-project` defines, as
 * that deck's `ResolveProject` answers them: one, `demo-loop`, whose
 * `planner` starts the run and whose `builder` works for it.
 *
 * `scratch/twin-project` is the audit F2 case: it defines `twin-loop` TWICE —
 * which a real config may, since validation only warns — and `solo-loop` once,
 * so the preview shows namesakes disabled beside an orchestration that can be
 * chosen. Every other directory in {@link fixtureDirectoryTree} is an ordinary
 * one.
 */
export function fixtureProjectOrchestrations(home: string, path: string): DaemonOrchestration[] | undefined {
  if (path === `${home}/scratch/twin-project`) {
    const single = (name: string, role: string): DaemonOrchestration => ({ name, displayName: name, default: false, roles: [{ name: role, displayName: role, start: true }] });
    return [single("twin-loop", "planner"), single("solo-loop", "planner"), single("twin-loop", "builder")];
  }
  if (path !== `${home}/demo-project`) return undefined;
  return [
    {
      name: "demo-loop",
      displayName: "demo-loop",
      default: true,
      roles: [
        { name: "planner", displayName: "planner", start: true },
        { name: "builder", displayName: "builder", start: false },
      ],
    },
  ];
}

/**
 * The commands `demo-loop`'s roles are configured with on a fixture deck — the
 * deck's own config, which never crosses the wire. The fixture deck starts each
 * role with its command the way a live deck does for a launch from the New
 * agent dialog, so the preview's role panes are labelled by what they run.
 */
export const FIXTURE_ROLE_COMMANDS: Readonly<Record<string, string>> = {
  planner: "claude",
  builder: "codex",
};

/**
 * The agent registry a fixture deck reports, shaped as `new-agent-options`
 * reports the real one: each entry's first basename as the id, its label, its
 * default command, in registry order. The fixture has no Rust to ask, so this
 * is preview data — a live deck answers with its own build's list, and an
 * older live deck's fallback comes from this app's Rust build, never from here.
 */
export function fixtureAgentRegistry(): NewAgentOption[] {
  return [
    { id: "claude", displayName: "ClaudeCode", defaultCommand: "claude" },
    { id: "opencode", displayName: "OpenCode", defaultCommand: "opencode" },
    { id: "pi", displayName: "Pi", defaultCommand: "pi" },
    { id: "codex", displayName: "Codex", defaultCommand: "codex" },
    { id: "devin", displayName: "Devin", defaultCommand: "devin" },
  ];
}

/**
 * The `default_command` each fixture deck's host configures, if any. The remote
 * deck has one and the local deck does not, so the preview shows both halves
 * of the Command prefill order.
 */
export const FIXTURE_DEFAULT_COMMANDS: Readonly<Record<string, string>> = {
  [FIXTURE_REMOTE_DAEMON_ID]: "claude",
};

/**
 * The fixture decks whose own experimental flag is on (PRD #1223 M7). The
 * remote deck's is and the local deck's is not, so the preview shows the
 * `schedule: issues` chip on one deck and withholds it on the other — the flag
 * is the deck's, not this app's.
 */
export const FIXTURE_EXPERIMENTAL_DECKS: ReadonlySet<string> = new Set([FIXTURE_REMOTE_DAEMON_ID]);

function orchestrationTab(orchestrationId: string, name: string, displayTitle: string, roleName: string, roleIndex: number, isStartRole = false, cwd?: string): AgentTab {
  return { kind: "orchestration", orchestrationId, name, displayTitle, roleName, roleIndex, isStartRole, cwd };
}

/*
 * Neutral home paths: a fixture capture is what ends up in a screenshot or a
 * demo recording, so it carries no real machine's username or directory layout.
 * They stay home-SHAPED on purpose, because that is what exercises the
 * overview's home-relative rendering (`~/code/...`) in the very captures where
 * an absolute home path would be the thing worth hiding.
 */
const DECK_CWD = "/home/dev/code/dot-agent-deck";
const PRD_CWD = "/home/dev/code/dot-agent-deck-dispatch-prd-745";

/**
 * Fifteen agents across two orchestrations, one mode bucket and three
 * standalone panes — the scale the overview exists for. Ids are per-daemon
 * monotonic integers because that is what the daemon mints, which is also why
 * nothing may key an agent by the bare id alone.
 *
 * Deliberately declared out of role order, and orchestration `dot-ai`'s start
 * role is deliberately NOT its first role, so grouping, ordering and
 * coordinator identification are three separate things a screen has to get
 * right rather than one accident of declaration order.
 */
const crowdedAgents: AgentSession[] = [
  crowdedAgent({ id: "2", displayName: "coder", role: "Coder", cli: "claude", status: "running", cwd: PRD_CWD, toolCount: 132, upForMinutes: 41, quietForMinutes: 0, activeTool: "edit", activeToolDetail: "desktop/src/components/AgentOverview.tsx", writeLease: "write", lastUserPrompt: "Surface the honest fields and stop presenting attempt and branch as facts.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "coder", 1, false, PRD_CWD) }),
  crowdedAgent({ id: "7", displayName: "writer", role: "Writer", cli: "claude", status: "running", cwd: DECK_CWD, toolCount: 19, upForMinutes: 96, quietForMinutes: 2, activeTool: "write", activeToolDetail: "docs/develop/desktop-gui.md", writeLease: "write", lastUserPrompt: "Document the overview screen and the demand-driven attach model.", tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "writer", 0, false, DECK_CWD) }),
  // No prompt and no lease: a pane the daemon adopted but that has emitted no
  // prompt event yet. Both columns stay blank, which is the case the screen has
  // to look right for.
  crowdedAgent({ id: "13", displayName: "Scratch shell", role: "Codex", cli: "codex", status: "waiting", cwd: DECK_CWD, toolCount: 2, tab: { kind: "dashboard" } }),
  crowdedAgent({ id: "1", displayName: "orchestrator", role: "Orchestrator", cli: "claude", status: "running", cwd: PRD_CWD, toolCount: 47, upForMinutes: 194, quietForMinutes: 0, activeTool: "read", activeToolDetail: "prds/done/745-desktop-agent-overview-landing-screen.md", writeLease: "write", lastUserPrompt: "Run PRD 745 to done: delegate each milestone and verify the gates yourself.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "orchestrator", 0, true, PRD_CWD) }),
  // History-only: a wrapped session the deck can replay but cannot type into.
  crowdedAgent({ id: "11", displayName: "Second opinion", role: "Open code", cli: "opencode", status: "running", cwd: DECK_CWD, toolCount: 33, upForMinutes: 12, quietForMinutes: 1, activeTool: "read", activeToolDetail: "src/state.rs", writeLease: "read", lastUserPrompt: "Read the daemon state module and tell me which fields never reach the desktop.", tab: { kind: "mode", name: "review" } }),
  crowdedAgent({ id: "5", displayName: "docs", role: "Docs", cli: "codex", status: "waiting", cwd: PRD_CWD, toolCount: 0, upForMinutes: 58, quietForMinutes: 34, writeLease: "write", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "docs", 4, false, PRD_CWD) }),
  crowdedAgent({ id: "9", displayName: "orchestrator", role: "Orchestrator", cli: "claude", status: "waiting", cwd: DECK_CWD, toolCount: 12, upForMinutes: 213, quietForMinutes: 8, writeLease: "write", lastUserPrompt: "Refresh the docs set for the release and hand each page to a reviewer.", tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "orchestrator", 2, true, DECK_CWD) }),
  crowdedAgent({ id: "4", displayName: "reviewer", role: "Reviewer", cli: "codex", status: "running", cwd: PRD_CWD, toolCount: 24, upForMinutes: 47, quietForMinutes: 3, activeTool: "grep", activeToolDetail: "attachAgents", writeLease: "write", lastUserPrompt: "Audit the attach path: prove the overview opens no socket, report findings only.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "reviewer", 3, false, PRD_CWD) }),
  crowdedAgent({ id: "14", displayName: "Changelog sweep", role: "Claude code", cli: "claude", status: "running", cwd: DECK_CWD, toolCount: 15, upForMinutes: 3, quietForMinutes: 0, activeTool: "bash", activeToolDetail: "git log --oneline -20", writeLease: "write", lastUserPrompt: "Collect every changelog fragment merged since the last tag and group them.", tab: { kind: "dashboard" } }),
  crowdedAgent({ id: "6", displayName: "release", role: "Release", cli: "claude", status: "failed", cwd: PRD_CWD, toolCount: 8, upForMinutes: 88, quietForMinutes: 71, activeTool: "bash", activeToolDetail: "cargo test-fast", writeLease: "write", lastUserPrompt: "Cut the release once the fast tier is green.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "release", 5, false, PRD_CWD) }),
  crowdedAgent({ id: "3", displayName: "tester", role: "Tester", cli: "codex", status: "waiting", cwd: PRD_CWD, toolCount: 61, upForMinutes: 33, quietForMinutes: 17, writeLease: "write", lastUserPrompt: "Write the failing test first, then hand it back without fixing it.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD 745 · agent dashboard", "tester", 2, false, PRD_CWD) }),
  crowdedAgent({ id: "10", displayName: "publisher", role: "Publisher", cli: "codex", status: "waiting", cwd: DECK_CWD, toolCount: 0, tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "publisher", 3, false, DECK_CWD) }),
  // View-only: the daemon knows the session but holds nothing it can write to.
  crowdedAgent({ id: "15", displayName: "pi-extension spike", role: "Pi", cli: "pi", status: "waiting", cwd: `${DECK_CWD}/pi-extension`, toolCount: 0, quietForMinutes: 2760, writeLease: "none", tab: { kind: "dashboard" } }),
  crowdedAgent({ id: "8", displayName: "reviewer", role: "Reviewer", cli: "codex", status: "waiting", cwd: DECK_CWD, toolCount: 5, upForMinutes: 168, quietForMinutes: 128, writeLease: "write", lastUserPrompt: "Review the docs refresh for accuracy against the current CLI flags.", tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "reviewer", 1, false, DECK_CWD) }),
  crowdedAgent({ id: "12", displayName: "Security pass", role: "Claude code", cli: "claude", status: "waiting", cwd: DECK_CWD, toolCount: 7, upForMinutes: 27, quietForMinutes: 9, tab: { kind: "mode", name: "review" } }),
];

/**
 * `empty` means CONNECTED WITH ZERO AGENTS — the first-run experience — and
 * not a daemon that is down. It used to fall through to the `disconnected`
 * branch, so `?fixture=1&state=empty` rendered a disconnected banner over an
 * empty deck and the genuine first-run screen had no fixture at all (PRD #745
 * M2). `disconnected` is the state that means nothing is listening.
 *
 * `crowded` is the same healthy connection carrying the fifteen-agent fleet.
 * It is opt-in rather than the default because changing the default would
 * churn the existing `App.test.tsx` suite for no gain.
 */

/**
 * The three decks `?fixture=1&state=fleet` shows, and the shape of the fleet is
 * the point of it (PRD #742 M4).
 *
 * Two connected and one not, because **partial connectivity is the state this
 * whole PRD is about**: a fixture where every deck answers would let a header
 * that sums over all decks look correct, and summing is exactly the failure to
 * avoid — a disconnected deck's fleet is unknown, not zero.
 *
 * The remote decks' agents deliberately reuse the local deck's agent **ids**.
 * Ids are per-daemon monotonic values, so two decks minting the same one is the
 * ORDINARY case rather than a contrived one, and a fixture that avoided the
 * collision would make every `(daemonId, agentId)` assertion pass for the wrong
 * reason.
 */
const remoteAgents: AgentSession[] = [
  {
    ...(agents[0] as AgentSession),
    daemonId: FIXTURE_REMOTE_DAEMON_ID,
    displayName: "Nightly build watch",
    role: "Builder",
    status: "running",
    task: "Keep the release branch green across the arm64 matrix.",
    cwd: "/home/dev/code/dot-agent-deck",
    transcript: "",
    handoffIds: [],
    artifacts: [],
    checks: [],
    tab: orchestrationTab("orc-release", "release-train", "Release train · arm64", "builder", 0, true, "/home/dev/code/dot-agent-deck"),
    inOrchestration: true,
    isStartRole: true,
  },
  {
    ...(agents[1] as AgentSession),
    daemonId: FIXTURE_REMOTE_DAEMON_ID,
    displayName: "Matrix reviewer",
    role: "Reviewer",
    status: "waiting",
    task: "Read the failing arm64 job and say whether it is the change or the runner.",
    cwd: "/home/dev/code/dot-agent-deck",
    transcript: "",
    handoffIds: [],
    artifacts: [],
    checks: [],
    tab: orchestrationTab("orc-release", "release-train", "Release train · arm64", "reviewer", 1, false, "/home/dev/code/dot-agent-deck"),
    inOrchestration: true,
    isStartRole: false,
  },
  {
    ...(agents[2] as AgentSession),
    daemonId: FIXTURE_REMOTE_DAEMON_ID,
    displayName: "Scratch shell",
    role: "Claude code",
    status: "failed",
    task: "Task metadata unavailable from the daemon",
    cwd: "/home/dev/code/scratch",
    transcript: "",
    handoffIds: [],
    artifacts: [],
    checks: [],
    tab: { kind: "dashboard" },
    inOrchestration: false,
    isStartRole: false,
  },
];

/**
 * What the unreachable deck was running when it last answered — see the comment
 * at its `fleetDeck` call, which is where the reason lives.
 */
const staleAgents: AgentSession[] = [
  {
    ...(agents[0] as AgentSession),
    daemonId: FIXTURE_UNREACHABLE_DAEMON_ID,
    displayName: "Integration sweep",
    role: "Tester",
    status: "running",
    task: "Run the integration tier against the staging cluster.",
    cwd: "/home/ci/code/dot-agent-deck",
    transcript: "",
    handoffIds: [],
    artifacts: [],
    checks: [],
    tab: { kind: "dashboard" },
  },
  {
    ...(agents[1] as AgentSession),
    daemonId: FIXTURE_UNREACHABLE_DAEMON_ID,
    displayName: "Flake triage",
    role: "Reviewer",
    status: "waiting",
    task: "Classify last night's flakes by whether they touch the PTY harness.",
    cwd: "/home/ci/code/dot-agent-deck",
    transcript: "",
    handoffIds: [],
    artifacts: [],
    checks: [],
    tab: { kind: "mode", name: "review" },
  },
];

/**
 * One deck of the fleet, built from the `connected` scenario so every field the
 * screens read is present and only the things that genuinely differ per deck —
 * identity, reachability and the agents — are overridden.
 */
function fleetDeck(
  daemonId: string,
  connection: DeckSnapshot["connection"],
  fleetAgents: AgentSession[],
  worktree: string,
): DeckSnapshot {
  const base = createFixtureSnapshot("connected");
  return {
    ...base,
    runId: `run_${daemonId.replace(/[^a-z0-9]+/gi, "_")}`,
    repo: worktree.split("/").filter(Boolean).at(-1) ?? worktree,
    worktree,
    connection,
    health: connection.status === "connected" ? (fleetAgents.some((agent) => agent.status === "failed") ? "failed" : "healthy") : "idle",
    agents: fleetAgents.map((agent) => ({ ...agent })),
    totalNodes: fleetAgents.length,
    // A deck nobody can reach has nothing to say about a run, and a screen that
    // showed one deck's stages under another deck's name would be the mislabel
    // the whole milestone is about.
    stages: connection.status === "connected" ? base.stages : [],
    evidence: connection.status === "connected" ? base.evidence : [],
    handoffs: connection.status === "connected" ? base.handoffs : [],
  };
}

/**
 * Every deck a scenario shows, SELECTED DECK FIRST — the fixture half of
 * `DeckBridge.connect()`'s contract (PRD #742 M4).
 *
 * Every scenario but `fleet` is one deck, byte for byte what
 * {@link createFixtureSnapshot} always returned, so nothing that existed before
 * this milestone changes. `fleet` is the one that needs the array to exist.
 */
export function createFixtureFleet(state: FixtureState = "connected"): DeckSnapshot[] {
  if (state !== "fleet") return [createFixtureSnapshot(state)];
  return [
    fleetDeck(
      FIXTURE_DAEMON_ID,
      { status: "connected", deckId: FIXTURE_DAEMON_ID, socketPath: FIXTURE_DAEMON_ID, message: "Daemon responding", deckKind: "local" },
      agents,
      "/home/dev/code/dot-agent-deck-gui",
    ),
    fleetDeck(
      FIXTURE_REMOTE_DAEMON_ID,
      { status: "connected", deckId: FIXTURE_REMOTE_DAEMON_ID, socketPath: FIXTURE_REMOTE_DAEMON_ID, message: "Daemon responding", deckKind: "remote", localOnlyReason: "Stop daemon acts on a process on this machine." },
      remoteAgents,
      "/home/dev/code/dot-agent-deck",
    ),
    /*
      The unreachable deck, and it deliberately carries the agents it was LAST
      SEEN running rather than none.

      That is what live mode does — a reconnect failure replaces the connection
      and keeps the previous snapshot's fleet — and it is the only version of
      this scenario that can tell a correct header from a wrong one: with an
      empty deck, summing over every deck and summing over the answering ones
      give the same number, so the test that is supposed to catch the
      under-count would pass against the bug. The screen still lists none of
      them, because a deck that stopped answering cannot vouch for what it was
      running; the stale list exists so the COUNT has something to be wrong
      about.
    */
    fleetDeck(
      FIXTURE_UNREACHABLE_DAEMON_ID,
      { status: "disconnected", deckId: FIXTURE_UNREACHABLE_DAEMON_ID, socketPath: FIXTURE_UNREACHABLE_DAEMON_ID, message: "No daemon is listening on the configured socket.", deckKind: "remote", localOnlyReason: "Stop daemon acts on a process on this machine." },
      staleAgents,
      "/home/dev/code/dot-agent-deck",
    ),
    /*
      PRD #742 M14: the deck that has not reported YET, and it carries no
      agents at all — which is the difference from the one above it.

      A deck that stopped answering was once seen running something, so the
      fixture gives it a stale list for the header to be wrong about. Nothing
      has ever been heard from this one, so an agent list would be a fiction
      with no live counterpart: `pendingDeckSnapshot` builds its group from a
      name and nothing else. The message is `PENDING_DECK_MESSAGE`'s wording,
      spelled out here rather than imported — `bridge.ts` imports this module,
      so importing it back would close a cycle for one string; the vitest suite
      asserts the two agree instead.

      What it IS for is the denominator: with it the scenario reads `2/4` rather
      than `2/3`, and a header that quietly dropped an unreported deck would
      read `2/3` here and look exactly as correct.
    */
    fleetDeck(
      FIXTURE_PENDING_DAEMON_ID,
      { status: "loading", deckId: FIXTURE_PENDING_DAEMON_ID, socketPath: FIXTURE_PENDING_DAEMON_ID, message: "In the fleet, waiting for it to report.", deckKind: "remote", pending: true },
      [],
      "/home/dev/code/dot-agent-deck",
    ),
  ];
}

export function createFixtureSnapshot(state: FixtureState = "connected"): DeckSnapshot {
  // `fleet` is a THREE-deck scenario and has no single snapshot, so a caller
  // asking for one gets the deck the single-deck screens are on — never the
  // disconnected fall-through an unlisted state would otherwise land in.
  if (state === "fleet") return createFixtureFleet(state)[0];
  const connected = state === "connected" || state === "crowded" || state === "empty";
  const connection = connected
    ? { status: "connected" as const, deckId: FIXTURE_DAEMON_ID, socketPath: FIXTURE_DAEMON_ID, message: state === "empty" ? "Daemon responding · no agents running" : "Daemon responding" }
    : state === "error"
      ? { status: "error" as const, message: "Protocol handshake failed. Desktop expects v6; daemon reported v5." }
      : { status: "disconnected" as const, message: "No daemon is listening on the configured socket." };

  const fleet = state === "empty" ? [] : state === "crowded" ? crowdedAgents : agents;

  return {
    runId: "run_7f24a",
    repo: "dot-agent-deck",
    branch: "codex/visual-control-deck",
    worktree: "/dev/active/dot-agent-deck-gui",
    connection,
    health: state === "connected" || state === "crowded" ? "healthy" : state === "error" ? "failed" : "idle",
    elapsed: "16:42",
    spend: 2.57,
    currentNode: 4,
    totalNodes: 6,
    currentAttempt: 1,
    paused: false,
    stages: state === "empty" ? [] : stages.map((stage) => ({ ...stage })),
    agents: fleet.map((agent) => ({ ...agent })),
    handoffs: [
      { id: "dlg-demo-3", toRole: "Reviewer", orchestration: "dot-agent-deck", taskPreview: "Review the terminal lifecycle change; report findings only.", status: "delegated", respawned: true, at: "14:41:22" },
      { id: "dlg-demo-2", toRole: "Builder", orchestration: "dot-agent-deck", taskPreview: "Implement the Tauri client as a second daemon surface.", status: "done", respawned: true, at: "14:40:58" },
      { id: "dlg-demo-1", toRole: "Tester", orchestration: "dot-agent-deck", taskPreview: "Write the failing bridge test for listener disposal.", status: "failed", respawned: false, reason: "worker respawn failed: command not found", at: "14:33:07" },
    ],
    evidence: state === "empty" ? [] : evidence.map((item) => ({ ...item })),
    profiles: DEFAULT_PROFILES.map((profile) => ({ ...profile })),
  };
}

/**
 * PRD #802 M6 — the deterministic stand-in for the whole voice backend.
 *
 * # Why the vocabulary lives HERE and not in the panel
 *
 * The browser tier has no Tauri runtime at all — `vite preview` serving static
 * files — so `FixtureDeckBridge` stands in for every Rust-side stage of the
 * pipeline: transcribe, resolve, validate, report. That makes this the fixture's
 * *model plus its command table*, which is fixture data, exactly like the
 * snapshots above. A panel that carried its own vocabulary would be a second
 * resolver, and the one property PRD #802 is built on — the app renders every
 * sentence the user reads, and renders it once — would be gone: the panel would
 * phrase in fixture mode what Rust phrases in live mode.
 *
 * # Exact matching, not substring or fuzzy
 *
 * A fixture is one instant held still, and a deterministic preview is worth
 * having precisely because it cannot surprise anybody. Matching is trim plus
 * lowercase and then equality, which is also what keeps the no-match path
 * reachable: `Go, BACK to "Deck"?!` has to resolve to nothing, and any
 * substring rule would have it matching the deck phrases below.
 *
 * # Every param-free row, and no other
 *
 * Every param-free row is here, so the fixture can answer it honestly with no
 * resolver of its own. `open_agent` is the one left out: it would need a second
 * `agent_ref` resolver here to reach at all — the real one, and every refusal
 * it produces, is covered in `voice/outcome.rs` — and inventing a stand-in for
 * it in a browser is the drift this module's placement is about.
 *
 * (This heading counted *three*, then four. It names the rule now rather than a
 * number, so the next row does not have to remember to edit a heading.)
 *
 * **`dictate_to_agent` used to be left out for `open_agent`'s reason and is
 * here now, because the param it declares changed shape.** It took an
 * `agent_ref`; it takes a `spoken_prefix`, which resolves against the
 * TRANSCRIPT rather than against live state. That is a rule a preview can
 * reproduce exactly — strip the opener, type the rest — where a resolver over
 * a live fleet is not. The row below says which half is reproduced and which
 * half (the model fallback) still is not.
 *
 * **`close` was left out until the voice surface could be reached on the
 * `agent` screen at all.** That screen's whole background — the voice trigger
 * included — was marked `inert` by the pane's modal fence, so a fixture row for
 * it answered a question nothing could ask. `useInertBackground` now exempts
 * the voice surface as a peer dialog, so the question is askable and the row is
 * here to answer it.
 */
const FIXTURE_VOICE_COMMANDS: ReadonlyArray<{
  readonly phrases: readonly string[];
  readonly action: string;
  readonly invoke: string;
  readonly screens: readonly VoiceScreen[];
  readonly unavailableHint: string;
  readonly report: string;
  /**
   * What a dispatch of this row carries, already resolved.
   *
   * **This is fixture DATA, not the `agent_ref` resolver the note above refuses
   * to invent**, and the line between them is worth stating because it is the
   * whole reason `open_agent` still has none. A resolver takes words nobody
   * anticipated and finds an agent; this is one canned answer for one canned
   * phrase, exactly like every sentence in this module. `open_agent`'s entire
   * interest IS the resolution — the ambiguity, the no-match, the reference by
   * state — which a canned answer cannot exercise and `voice/outcome.rs`
   * already covers properly. `dictate_to_agent`'s interest is what happens
   * AFTER a param resolves, which is precisely what a canned one lets the
   * browser tier drive.
   */
  readonly params?: readonly VoiceResolvedParamDto[];
  /**
   * Words this row is matched by as a PREFIX rather than by equality, with
   * everything after them becoming the row's `spoken_prefix` param.
   *
   * The one departure from equality matching in this module, and it reproduces
   * the real fast path's rule rather than inventing one — see the dictation row
   * below for why that is a different thing from inventing an `agent_ref`
   * resolver.
   */
  readonly openers?: readonly string[];
}> = [
  {
    phrases: ["show me every agent", "show me all the agents", "show me everything"],
    action: "open_overview",
    invoke: "openOverview",
    screens: ["deck", "overview"],
    unavailableHint: "the agent dashboard opens from the rail once the agent's pane is closed",
    report: "Opening the agent dashboard.",
  },
  {
    phrases: ["open daemons", "go back to the daemons", "back to the daemons", "go back to the deck", "back to the deck", "show me the terminals"],
    action: "open_deck",
    invoke: "openDeck",
    screens: ["deck", "overview"],
    unavailableHint: "the Daemons screen opens from the rail once the agent's pane is closed",
    report: "Back to the Daemons screen.",
  },
  {
    // Both screens, because the one rail offers Settings on both (#1197).
    phrases: ["open settings", "settings", "open the settings"],
    action: "open_settings",
    invoke: "openSettings",
    screens: ["deck", "overview"],
    unavailableHint: "settings open from the rail once the agent's pane is closed",
    report: "Opening settings.",
  },
  {
    // The VIEW, never the terminal pane — the same line `commands.toml` draws at
    // this row, because the two are one word apart in speech. Callable on all
    // three screens for `voice_off`'s reason: the real row has no `screens`
    // column, because the overlay it dismisses can be up over any of them.
    phrases: ["close this", "close", "close the agent view", "stop looking at this one"],
    action: "close",
    invoke: "closeTopmost",
    screens: ["deck", "overview", "agent"],
    unavailableHint: "closing what is on top works anywhere",
    report: "Closed.",
  },
  {
    // The only entry listing all three screens, because the real row lists
    // NONE — an absent `screens` column means "everywhere". The fixture's own
    // matcher is `screens.includes(screen)`, so an empty array here would mean
    // the opposite of what an empty column means in `commands.toml`; spelling
    // the three out is what keeps the preview and the table agreeing.
    phrases: ["voice off", "turn off the voice", "stop listening"],
    action: "voice_off",
    invoke: "stopVoice",
    screens: ["deck", "overview", "agent"],
    unavailableHint: "turning voice off works anywhere",
    report: "Voice control off.",
  },
  {
    // Callable everywhere for the same reason, and listing ITSELF in the
    // overlay it opens — which is correct rather than cute: a user who has
    // forgotten the phrase is exactly who reads that list.
    /* Lower case, because the matcher lowercases the utterance before
       comparing: a phrase with a capital in it here can never be matched. */
    phrases: ["what can i say?", "what can i say", "what can you do?", "help"],
    action: "list_commands",
    invoke: "showVoiceCommands",
    screens: ["deck", "overview", "agent"],
    unavailableHint: "the list of commands opens anywhere",
    report: "Here is what you can say.",
  },
  {
    // **Matched by OPENER rather than by phrase**, which is the one place this
    // module reproduces a rule instead of canning an answer — and it is
    // allowed to for the reason `agent_ref` is not: the fast path's rule is
    // local, deterministic and complete (`voice::dictation::strip_opening`
    // strips the opener and types the rest), so reproducing it invents
    // nothing. What a fixture could not stand in for is the MODEL fallback,
    // which is why the openers here are exactly the four the real fast path
    // knows and no more.
    //
    // `screens: ["agent"]` matches the real row: the target is the pane on
    // screen, so the preview's dictation tests open one first. The crowded
    // fleet's `coder` is the pane they open, and that choice is the feature's
    // own requirement rather than an arbitrary pick — dictation types into a
    // pane, so the target has to be an agent whose pane accepts typing. Every
    // agent in the `connected` deck fails that (`planner` and `reviewer` hold a
    // read lease, `tester` has no live target, `builder` has finished) while
    // the crowded fleet's `coder` is running and holds a write lease.
    phrases: [],
    openers: ["type", "write", "say", "dictate"],
    action: "dictate_to_agent",
    invoke: "dictateToAgent",
    screens: ["agent"],
    unavailableHint: "typing to an agent needs that agent's pane open — open one first",
    report: "Typed.",
  },
  {
    // Whole-utterance equality, which is what the phrase matcher already is —
    // the real fast path draws the same line, and for the reason its own
    // constant documents at length: a trailing rule would submit "the meeting
    // is at the" when somebody said "type the meeting is at the end".
    phrases: ["end", "send", "send it", "submit", "enter", "press enter"],
    action: "submit_prompt",
    invoke: "submitAgentPrompt",
    screens: ["agent"],
    unavailableHint: "sending a prompt needs an agent's pane open — open one first",
    report: "Sent.",
  },
];

/**
 * The preview's vocabulary, annotated for one screen (PRD #802 D7).
 *
 * Derived from {@link FIXTURE_VOICE_COMMANDS} rather than written out, exactly
 * as the live one is derived from `commands.toml`: the overlay lists what this
 * bridge can actually resolve, so a row added above appears in it with no edit
 * here.
 *
 * `description` is the one field the preview has to invent, because the fixture
 * rows carry phrases rather than a prompt. It names the phrases, which is
 * honest about what this stand-in is — a matcher over a fixed list — and is
 * what a preview reader most needs to know.
 */
export function fixtureVoiceCommands(screen: VoiceScreen): VoiceCommandDto[] {
  return FIXTURE_VOICE_COMMANDS.map((command) => ({
    id: command.action,
    description: `Say ${command.phrases.map((phrase) => `“${phrase}”`).join(", ")}.`,
    callable: command.screens.includes(screen),
    unavailable_hint: command.unavailableHint,
    params: [],
  }));
}

/**
 * `Heard: “<transcript>” — <situation>.`
 *
 * The transcript goes in verbatim, exactly as `voice::outcome`'s `heard` does:
 * seeing what was heard is what turns a mis-transcription into a correction the
 * user can make, so this is not the seam that bounds or scrubs it. The webview
 * scrubs its own display copy at the render seam.
 */
function fixtureHeard(transcript: string, situation: string): string {
  return `Heard: “${transcript}” — ${situation}.`;
}

/**
 * Resolve one utterance the way the Rust pipeline would, against the screen the
 * webview has stated.
 *
 * `resolveMs` is `null` and the backend is `stub` because **no backend was
 * called** — there is none in a browser. Reporting a plausible number here
 * would be the preview inventing a measurement, which is the same fabrication
 * `resolve_ms: None` exists to refuse on the Rust side.
 */
export function resolveFixtureVoice(utterance: string, screen: VoiceScreen): VoiceResultDto {
  const spoken = utterance.trim().toLowerCase();
  const stub = { resolveMs: null, backend: "stub" } as const;
  const command = FIXTURE_VOICE_COMMANDS.find((candidate) => candidate.phrases.includes(spoken))
    ?? FIXTURE_VOICE_COMMANDS.find((candidate) => (candidate.openers ?? []).some((opener) => fixtureOpening(utterance, opener) !== undefined));
  if (!command) {
    return { ...stub, outcome: { kind: "no_match", transcript: utterance, sentence: fixtureHeard(utterance, "no matching action") } };
  }
  if (!command.screens.includes(screen)) {
    return {
      ...stub,
      outcome: { kind: "unavailable", transcript: utterance, action: command.action, hint: command.unavailableHint, sentence: `Not here — ${command.unavailableHint}.` },
    };
  }
  /* The typed text is a slice of the UTTERANCE, never of anything this module
     made up — which is the property the real pipeline holds and the preview
     would be misleading about if it canned a string here. */
  const opener = (command.openers ?? []).find((candidate) => fixtureOpening(utterance, candidate) !== undefined);
  const text = opener === undefined ? undefined : fixtureOpening(utterance, opener);
  const params: VoiceResolvedParamDto[] = text === undefined
    ? [...command.params ?? []]
    : [{ name: "prefix", kind: "spoken_prefix", spoken: opener!, value: text, label: text }];
  return {
    ...stub,
    outcome: {
      kind: "dispatch",
      transcript: utterance,
      action: command.action,
      invoke: command.invoke,
      params,
      sentence: text === undefined ? command.report : `Typed: “${text}”.`,
    },
  };
}

/**
 * Everything after `opener`, or `undefined` if the utterance does not open with
 * it — the preview's copy of `voice::dictation::strip_opening`, matched on
 * whole words so *"typescript is confusing"* does not open with *"type"*.
 *
 * A remainder of nothing is `undefined` too: *"type"* alone is not a dictation,
 * exactly as it is not one in Rust, where it falls through to the model.
 */
function fixtureOpening(utterance: string, opener: string): string | undefined {
  const words = utterance.trim();
  const lowered = words.toLowerCase();
  const marked = opener.toLowerCase();
  if (!lowered.startsWith(marked)) return undefined;
  const rest = words.slice(marked.length);
  if (rest !== "" && /[\p{L}\p{N}]/u.test(rest[0])) return undefined;
  const text = rest.replace(/^[\s:,-]+/u, "");
  return text === "" ? undefined : text;
}

/**
 * The bound a live capture is held to, echoed so the preview reports the same
 * shape rather than a made-up one. Keep identical to `voice::MAX_UTTERANCE`.
 */
const FIXTURE_VOICE_MAX_MS = 30_000;

/**
 * The microphone the browser preview does not have (PRD #802 M7).
 *
 * With no overrides this reports a runtime that offers no microphone path at
 * all, which is what the browser preview genuinely is: no Rust side, no
 * container, and a CSP that leaves the webview unable to reach a network
 * origin. That lets the browser tier drive the *unavailable* path with no
 * microphone, no credential and no Tauri runtime anywhere near it — and the
 * surface says how to turn voice on rather than looking broken.
 *
 * It is no longer what a live app reports on its defaults. PRD #802's provider
 * work gave Speech a keyless container on loopback and deleted `off`, so the
 * Tauri side now always reports `available: true` and the *not set up* case is
 * reported where it bites, as a `not_configured` transcription outcome naming
 * the container to start.
 *
 * The overrides are what lets the same tier drive the OTHER path — see
 * {@link fixtureVoiceHeard}.
 */
export function fixtureVoiceStatus(overrides: Partial<VoiceStatusDto> = {}): VoiceStatusDto {
  return {
    state: "idle",
    capturedMs: 0,
    maxMs: FIXTURE_VOICE_MAX_MS,
    capped: false,
    // The preview has no microphone to hear speech with, so it reports none.
    // An override is how a test drives the other answer — which is what PRD
    // #802's dictation countdown is cancelled by.
    speech: false,
    available: false,
    // The backend that WOULD answer, which for the preview is the app's own
    // default — nothing does, and `available: false` is what says so. `off`
    // stood here until the union was corrected, naming a variant PRD #802's
    // provider work deleted and the wire has never carried.
    backend: "local",
    ...overrides,
  };
}

/**
 * What the preview says when something asks it to listen (PRD #802 M7's
 * `TranscriptionOutcome::NotConfigured`).
 *
 * `not_configured` rather than `failed`, and the distinction is the point: the
 * preview has no microphone because it is a browser, which is not a fault to
 * report. `transcribeMs` is `null` for the reason `resolveFixtureVoice`'s is.
 */
export function fixtureVoiceTranscription(): VoiceTranscriptionDto {
  return {
    outcome: {
      kind: "not_configured",
      detail: "the browser preview has no microphone",
      sentence: "Nothing to listen with — the browser preview has no microphone.",
    },
    transcribeMs: null,
    backend: "off",
    audioMs: 0,
  };
}

/**
 * The DEFAULT line the preview's simulated microphone says.
 *
 * A phrase from {@link FIXTURE_VOICE_COMMANDS} rather than a fresh literal, so
 * a row renamed there changes what the preview hears instead of leaving a
 * canned utterance that quietly stops resolving.
 *
 * It is not the only one it can say: `?voice=` on the preview URL supplies a
 * whole script of utterances instead (see {@link fixtureVoiceScript}).
 */
export const FIXTURE_VOICE_UTTERANCE = FIXTURE_VOICE_COMMANDS[0].phrases[0];

/**
 * What the preview's microphone will say, in order, read off the URL.
 *
 * Repeated `?voice=` parameters rather than one delimited value: an utterance
 * is a sentence and every delimiter worth choosing occurs inside one. The
 * browser tier drives a whole session this way — *"type run the login tests"*,
 * then *"send it"* — which is what lets a Playwright test ask about the
 * utterance AFTER the first one, and a stand-in that spoke once could not be
 * asked that at all.
 *
 * With no parameter it is the single canned utterance the preview has always
 * had, so every existing page and test sees exactly what it saw before.
 */
export function fixtureVoiceScript(search: string): string[] {
  const spoken = new URLSearchParams(search).getAll("voice").filter((phrase) => phrase.trim() !== "");
  return spoken.length > 0 ? spoken : [FIXTURE_VOICE_UTTERANCE];
}

/**
 * One simulated utterance, so the browser tier can drive the WHOLE voice loop
 * (PRD #802 M6).
 *
 * The preview still has no microphone. What it has is a deterministic stand-in
 * for one: with a transcription backend chosen in its settings, a start is
 * accepted, the next status poll reports the utterance over, and this is what a
 * stop returns. That is the segment → transcribe → resolve → execute → listen
 * again cycle end to end, driven with no credential and no device — which is
 * what the withdrawn typed path used to be the only way to reach.
 *
 * `transcribeMs` is `null` and the backend is `stub` for `resolveFixtureVoice`'s
 * reason: nothing was transcribed, so there is no measurement to report.
 */
export function fixtureVoiceHeard(transcript: string = FIXTURE_VOICE_UTTERANCE): VoiceTranscriptionDto {
  return {
    outcome: {
      kind: "heard",
      transcript,
      sentence: `Heard: “${transcript}”.`,
    },
    transcribeMs: null,
    backend: "stub",
    audioMs: 1_200,
  };
}
