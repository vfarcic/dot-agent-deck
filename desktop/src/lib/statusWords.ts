import type { AgentStatus, DaemonAgentStatus } from "../types";
import { statusFromDaemon } from "./bridge";

/**
 * Issue #2411: a status as the user reads it. `AgentStatus` merges the daemon's
 * words into columns the deck reasons with (`running` for Thinking, Working and
 * Compacting, `failed` for Error); everything that SHOWS a status shows this
 * word instead — the daemon's own, which is also the TUI's (`status_style` in
 * `src/ui.rs`, CLAUDE.md rule 22). Merging states for display is the daemon's
 * call, so a client never does it.
 *
 * `running` stays for an agent the daemon has no hook state for yet (the dto's
 * fallback); `queued`, `passed` and `stopped` are the desktop's own states,
 * which no daemon word names.
 */
export type StatusWord = "thinking" | "working" | "compacting" | "running" | "needs_input" | "error" | "blocked" | "waiting" | "idle" | "queued" | "passed" | "stopped";

/** The word for each daemon status: its own, except the two the TUI also renames (`waiting_for_input` reads Needs Input, `unknown` reads Idle). */
const DAEMON_WORD: Record<DaemonAgentStatus, StatusWord> = {
  running: "running",
  thinking: "thinking",
  working: "working",
  compacting: "compacting",
  waiting_for_input: "needs_input",
  waiting: "waiting",
  idle: "idle",
  error: "error",
  blocked: "blocked",
  unknown: "idle",
};

/**
 * The word a column stands for when the agent carries no daemon word, which
 * only a fixture's agent does: `running` stands for Working there, as the
 * fixture's `turn` and the dashboard filter's facts already read it.
 */
const COLUMN_WORD: Record<AgentStatus, StatusWord> = {
  queued: "queued",
  running: "working",
  needs_input: "needs_input",
  waiting: "waiting",
  idle: "idle",
  passed: "passed",
  failed: "error",
  stopped: "stopped",
  blocked: "blocked",
};

/**
 * The word an agent's status is shown as. The daemon's word, unless the deck
 * has since moved the agent to a column that word does not belong to (a
 * desktop-only state), in which case that column's word.
 */
export function statusWord(agent: { status: AgentStatus; daemonStatus?: DaemonAgentStatus | string }): StatusWord {
  const daemon = agent.daemonStatus?.toLowerCase();
  const word = daemon !== undefined && statusFromDaemon(daemon) === agent.status ? DAEMON_WORD[daemon as DaemonAgentStatus] : undefined;
  return word ?? COLUMN_WORD[agent.status];
}

/**
 * The words a status is shown as. Each is its own id except Needs input, whose
 * id cannot carry a space; the TUI's cards say Needs Input and Idle for the
 * same two states.
 */
export function statusWordLabel(word: StatusWord): string {
  return word === "needs_input" ? "needs input" : word;
}

/**
 * Statuses in the order an operator scans them: the working ones in the TUI
 * stats bar's order, then what needs attention, then Waiting (issue #1757: its
 * work is not done, but nothing needs the user), then Idle (issue #1676), then
 * the desktop's own states.
 */
export const STATUS_WORD_ORDER: readonly StatusWord[] = ["working", "thinking", "compacting", "running", "needs_input", "error", "blocked", "waiting", "idle", "queued", "passed", "stopped"];
