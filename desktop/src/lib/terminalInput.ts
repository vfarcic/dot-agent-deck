import { sendResultReason } from "../types";
import type { AgentSession, SendResult } from "../types";

/**
 * Issue #1042 — whether an agent's terminal will accept what is typed into it,
 * and what to say when it will not.
 *
 * This is the mechanism the per-tile composer used to own, relocated onto the
 * one input path that has the agent CLI's whole grammar. It is deliberately
 * HYBRID, because one of the three conditions #1042 names cannot be known
 * without sending:
 *
 * - `history-only` and `no-live-target` are read off the snapshot's
 *   `writeLease`, which the desktop crate projects from
 *   `SessionSnapshot.live_target.writable` and pushes on every change, so they
 *   need no send and do not freeze at their mount value.
 * - `wrong-session` is decided inside the guarded send, by comparing the
 *   identity the caller queued against the identity the pane now owns
 *   (`src/agent_pty.rs`). The snapshot carries no expected identity to compare,
 *   and after a rollover the pane usually reads `writeLease === "write"` — it
 *   looks deliverable precisely when a send would fail. So it arrives as a
 *   post-hoc verdict from a programmatic send and nowhere else.
 */
export interface TerminalInputState {
  /**
   * The pane's input-acceptance state in the `SendResult` vocabulary, or
   * `undefined` when nothing about the PANE is being claimed — which is what
   * the agent-status gate below produces. Rendered as `data-input-state`.
   */
  state?: SendResult;
  /** True when the terminal must not accept keystrokes. */
  readOnly: boolean;
  /** The operator-facing sentence, or nothing when input is accepted silently. */
  notice?: string;
  /** How the notice reads: a hard block, or a delivery nobody could confirm. */
  tone: "open" | "blocked" | "unconfirmed";
}

/**
 * The delivery states that mean the pane is DEMONSTRABLY unwritable, and the
 * only ones that disable the input.
 *
 * `stale`, `ambiguous` and `unknown` are deliberately absent: each means
 * delivery was not *confirmed*, not that the pane is dead, and the pane may be
 * perfectly writable. Falsely blocking a working input is a worse failure than
 * an unconfirmed warning, and #1042's goal is not accepting keystrokes into a
 * KNOWN void. They warn instead — see {@link terminalInputState}.
 */
const UNWRITABLE: readonly SendResult[] = ["history-only", "no-live-target", "wrong-session"];

/**
 * The agent-status gate, moved here verbatim from `composerDisabledReason`.
 *
 * It says nothing about the pane — an agent that has not started, has finished,
 * or was stopped has no work in progress to type at, whatever its lease
 * reports. `TerminalViewport`'s `readOnly` used to carry an inline copy of this
 * condition and the composer carried the strings; both now read this one
 * function, so the two can no longer disagree.
 */
export function agentStatusInputReason(agent: AgentSession): string | undefined {
  if (agent.status === "queued") return "This agent has not started yet.";
  if (agent.status === "passed") return "This agent has finished its work.";
  if (agent.status === "stopped") return "This agent is stopped.";
  return undefined;
}

/**
 * The lease half: what the snapshot alone proves about this pane.
 *
 * `"unknown"` (and an absent lease) is the LEGACY LIVE DEFAULT and means
 * writable — the desktop crate omits the key when the daemon declared no
 * `live_target`, and every native PTY historically declared none. Reading it as
 * read-only would falsely disable the input on those daemons, so only `"read"`
 * and `"none"` produce a state here.
 */
function leaseState(agent: AgentSession): SendResult | undefined {
  if (agent.writeLease === "read") return "history-only";
  if (agent.writeLease === "none") return "no-live-target";
  return undefined;
}

/** The post-hoc half: a returned verdict, minus the two that mean delivery. */
function verdictState(verdict: SendResult | undefined): SendResult | undefined {
  if (verdict === undefined || verdict === "applied" || verdict === "queued") return undefined;
  return verdict;
}

/**
 * Resolve one agent's terminal-input state.
 *
 * `verdict` is the last non-delivered `SendResult` the guarded send verb
 * returned for this agent, if any; the lease outranks it, because a lease that
 * says the pane is gone is current state while a verdict is a record of one
 * past attempt.
 */
export function terminalInputState(agent: AgentSession, verdict?: SendResult): TerminalInputState {
  const reported = leaseState(agent) ?? verdictState(verdict);

  if (reported && UNWRITABLE.includes(reported)) {
    return {
      state: reported,
      readOnly: true,
      tone: "blocked",
      notice: `Terminal input unavailable — ${sendResultReason(reported)}.`,
    };
  }

  // Checked after the pane, not before it: for an agent that is both finished
  // and paneless, "the agent has no live pane" is the sentence that explains
  // why typing cannot work, which is what #1042 asked the terminal to say.
  const statusReason = agentStatusInputReason(agent);
  if (statusReason) return { state: undefined, readOnly: true, tone: "blocked", notice: statusReason };

  if (reported) {
    return {
      state: reported,
      readOnly: false,
      tone: "unconfirmed",
      notice: `Delivery was not confirmed — ${sendResultReason(reported)}.`,
    };
  }

  return { state: "applied", readOnly: false, tone: "open" };
}
