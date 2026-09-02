import type { AgentProfile, AgentSession, AgentStatus, AgentTab, DeckSnapshot, EvidenceItem, WorkflowStage } from "../types";

/**
 * The fixture's stand-in for a daemon identity. Live mode uses the daemon's
 * socket path, which is the only thing the handshake gives us that actually
 * distinguishes one local daemon from another; the fixture names the socket a
 * default deployment listens on.
 */
export const FIXTURE_DAEMON_ID = "/tmp/dot-agent-deck.sock";

/** Which scenario `createFixtureSnapshot` builds; selected by `?state=`. */
export type FixtureState = "connected" | "disconnected" | "error" | "empty" | "crowded";

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
    summary: "The plan limits this milestone to the daemon client, terminal deck, agent profiles, and observable loop.",
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
    artifacts: [{ id: "prd", name: "Desktop GUI PRD", kind: "file", path: "prds/176-desktop-gui.md" }],
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
    artifacts: [{ id: "cast", name: "Control deck smoke run", kind: "recording", path: ".dot-agent-deck/recordings/gui.cast" }],
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
        : "Task metadata unavailable from daemon"),
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
  crowdedAgent({ id: "2", displayName: "coder", role: "Coder", cli: "claude", status: "running", cwd: PRD_CWD, toolCount: 132, upForMinutes: 41, quietForMinutes: 0, activeTool: "edit", activeToolDetail: "desktop/src/components/AgentOverview.tsx", writeLease: "write", lastUserPrompt: "Surface the honest fields and stop presenting attempt and branch as facts.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "coder", 1, false, PRD_CWD) }),
  crowdedAgent({ id: "7", displayName: "writer", role: "Writer", cli: "claude", status: "running", cwd: DECK_CWD, toolCount: 19, upForMinutes: 96, quietForMinutes: 2, activeTool: "write", activeToolDetail: "docs/develop/desktop-gui.md", writeLease: "write", lastUserPrompt: "Document the overview screen and the demand-driven attach model.", tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "writer", 0, false, DECK_CWD) }),
  // No prompt and no lease: a pane the daemon adopted but that has emitted no
  // prompt event yet. Both columns stay blank, which is the case the screen has
  // to look right for.
  crowdedAgent({ id: "13", displayName: "Scratch shell", role: "Codex", cli: "codex", status: "waiting", cwd: DECK_CWD, toolCount: 2, tab: { kind: "dashboard" } }),
  crowdedAgent({ id: "1", displayName: "orchestrator", role: "Orchestrator", cli: "claude", status: "running", cwd: PRD_CWD, toolCount: 47, upForMinutes: 194, quietForMinutes: 0, activeTool: "read", activeToolDetail: "prds/745-desktop-agent-overview-landing-screen.md", writeLease: "write", lastUserPrompt: "Run PRD #745 to done: delegate each milestone and verify the gates yourself.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "orchestrator", 0, true, PRD_CWD) }),
  // History-only: a wrapped session the deck can replay but cannot type into.
  crowdedAgent({ id: "11", displayName: "Second opinion", role: "Open code", cli: "opencode", status: "running", cwd: DECK_CWD, toolCount: 33, upForMinutes: 12, quietForMinutes: 1, activeTool: "read", activeToolDetail: "src/state.rs", writeLease: "read", lastUserPrompt: "Read the daemon state module and tell me which fields never reach the desktop.", tab: { kind: "mode", name: "review" } }),
  crowdedAgent({ id: "5", displayName: "docs", role: "Docs", cli: "codex", status: "waiting", cwd: PRD_CWD, toolCount: 0, upForMinutes: 58, quietForMinutes: 34, writeLease: "write", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "docs", 4, false, PRD_CWD) }),
  crowdedAgent({ id: "9", displayName: "orchestrator", role: "Orchestrator", cli: "claude", status: "waiting", cwd: DECK_CWD, toolCount: 12, upForMinutes: 213, quietForMinutes: 8, writeLease: "write", lastUserPrompt: "Refresh the docs set for the release and hand each page to a reviewer.", tab: orchestrationTab("orc-dot-ai", "dot-ai", "dot-ai · docs refresh", "orchestrator", 2, true, DECK_CWD) }),
  crowdedAgent({ id: "4", displayName: "reviewer", role: "Reviewer", cli: "codex", status: "running", cwd: PRD_CWD, toolCount: 24, upForMinutes: 47, quietForMinutes: 3, activeTool: "grep", activeToolDetail: "attachAgents", writeLease: "write", lastUserPrompt: "Audit the attach path: prove the overview opens no socket, report findings only.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "reviewer", 3, false, PRD_CWD) }),
  crowdedAgent({ id: "14", displayName: "Changelog sweep", role: "Claude code", cli: "claude", status: "running", cwd: DECK_CWD, toolCount: 15, upForMinutes: 3, quietForMinutes: 0, activeTool: "bash", activeToolDetail: "git log --oneline -20", writeLease: "write", lastUserPrompt: "Collect every changelog fragment merged since the last tag and group them.", tab: { kind: "dashboard" } }),
  crowdedAgent({ id: "6", displayName: "release", role: "Release", cli: "claude", status: "failed", cwd: PRD_CWD, toolCount: 8, upForMinutes: 88, quietForMinutes: 71, activeTool: "bash", activeToolDetail: "cargo test-fast", writeLease: "write", lastUserPrompt: "Cut the release once the fast tier is green.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "release", 5, false, PRD_CWD) }),
  crowdedAgent({ id: "3", displayName: "tester", role: "Tester", cli: "codex", status: "waiting", cwd: PRD_CWD, toolCount: 61, upForMinutes: 33, quietForMinutes: 17, writeLease: "write", lastUserPrompt: "Write the failing test first, then hand it back without fixing it.", tab: orchestrationTab("orc-745", "dot-agent-deck", "PRD #745 · agent overview", "tester", 2, false, PRD_CWD) }),
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
export function createFixtureSnapshot(state: FixtureState = "connected"): DeckSnapshot {
  const connected = state === "connected" || state === "crowded" || state === "empty";
  const connection = connected
    ? { status: "connected" as const, socketPath: FIXTURE_DAEMON_ID, message: state === "empty" ? "Daemon responding · no agents running" : "Daemon responding" }
    : state === "error"
      ? { status: "error" as const, message: "Protocol handshake failed. Desktop expects v6; daemon reported v5." }
      : { status: "disconnected" as const, message: "No dot-agent-deck daemon is listening on the configured socket." };

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
      { id: "dlg-demo-3", toRole: "Reviewer", orchestration: "dot-agent-deck", taskPreview: "Review the terminal lifecycle change; report findings only.", status: "dispatched", respawned: true, at: "14:41:22" },
      { id: "dlg-demo-2", toRole: "Builder", orchestration: "dot-agent-deck", taskPreview: "Implement the Tauri client as a second daemon surface.", status: "done", respawned: true, at: "14:40:58" },
      { id: "dlg-demo-1", toRole: "Tester", orchestration: "dot-agent-deck", taskPreview: "Write the failing bridge test for listener disposal.", status: "failed", respawned: false, reason: "worker respawn failed: command not found", at: "14:33:07" },
    ],
    evidence: state === "empty" ? [] : evidence.map((item) => ({ ...item })),
    profiles: DEFAULT_PROFILES.map((profile) => ({ ...profile })),
  };
}
