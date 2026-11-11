import type { ConnectionView } from "../types";
import { deckName } from "./displayText";

/**
 * Issue #1073 — Stop daemon on one deck, which need not be the selected one:
 * whether the control is offered on it, and if it is offered but cannot act,
 * the crate's sentence for why.
 *
 * The local deck is offered whenever a daemon answers, compatible or not —
 * the crate falls back to a stop that reaches a daemon of any version. A
 * remote deck is offered only while it is connected, and carries
 * `stopDaemonReason` when its daemon is too old to be stopped from the app.
 * A deck with no id has nothing the action could name.
 */
export function stopDaemonOffer(connection: ConnectionView): { offered: false } | { offered: true; reason?: string } {
  if (connection.deckId === undefined) return { offered: false };
  if (connection.deckKind === "remote") {
    return connection.status === "connected" ? { offered: true, reason: connection.stopDaemonReason } : { offered: false };
  }
  return connection.status === "connected" || connection.daemonDetected === true ? { offered: true } : { offered: false };
}

/** Where a Stop daemon confirmation says the daemon runs: this machine, or the deck's name. */
function where(connection: ConnectionView): string {
  return connection.deckKind === "remote" ? deckName(connection) : "this machine";
}

/** The confirmation's title and body for stopping `connection`'s daemon. */
export function stopDaemonConfirmCopy(connection: ConnectionView): { title: string; body: string } {
  const liveAgents = connection.runningAgentCount;
  return {
    title: `Stop the daemon on ${where(connection)}?`,
    body: liveAgents && liveAgents > 0
      ? `The daemon reports ${liveAgents} live agent${liveAgents === 1 ? "" : "s"}. The stop will be refused until ${liveAgents === 1 ? "that agent is" : "those agents are"} closed.`
      : `This stops the daemon on ${where(connection)}. No live agents are reported, so this only shuts down the daemon.`,
  };
}
