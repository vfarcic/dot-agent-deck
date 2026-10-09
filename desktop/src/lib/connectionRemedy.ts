import type { ConnectionView } from "../types";

/** The Connect anyway confirmation, shared by the Daemons screen and the dashboard so the two cannot drift. */
export const CONNECT_ANYWAY_BODY = "This daemon and this app are different versions, so this app may show some of this daemon's information wrongly. Agent Deck will connect and keep a warning on screen until you quit the app; nothing is remembered after that.";

/** Issue #1490 — the Start daemon confirmation, shared by the Daemons screen and the dashboard. `host` is the reason's, verbatim. */
export function startDaemonConfirmCopy(host: string): { title: string; body: string } {
  return {
    title: `Start the daemon on ${host}?`,
    body: `Agent Deck will start the daemon on ${host} and connect to it. No agent is started until you explicitly create one or activate an orchestration.`,
  };
}

/**
 * Issue #1490 — which single remedy a disconnected deck offers. The desktop
 * crate decides it (`disconnectedReason.action`); a deck without a reason —
 * fixture data, or an older snapshot — gets `fallback`.
 */
export function disconnectedRemedy(connection: ConnectionView, fallback: "start-daemon" | "reconnect"): "start-daemon" | "reconnect" {
  return connection.disconnectedReason?.action ?? fallback;
}

/**
 * PR #1623 review — the technical half of a disconnected deck, for its
 * disclosure: the reason's own detail, the connection's error detail and, with
 * `message`, the connection's own error — the untrusted socket, the refused
 * handshake, the tunnel failure — which the reason's sentence replaces as the
 * headline and must not make disappear. Each once, and never the headline
 * itself. A screen that already shows the connection's error elsewhere passes
 * `message: false`.
 */
export function disconnectedDetails(connection: ConnectionView, options: { message: boolean }): string[] {
  const reason = connection.disconnectedReason;
  const details: string[] = [];
  const add = (text: string | undefined) => {
    const trimmed = text?.trim();
    if (trimmed && trimmed !== reason?.message.trim() && !details.includes(trimmed)) details.push(trimmed);
  };
  if (options.message && reason) add(connection.message);
  add(connection.detail);
  add(reason?.detail);
  return details;
}

/**
 * Which recovery buttons a screen actually renders beside an incompatible
 * daemon's message. Each screen offers a different set — the Daemons screen's
 * banner can replace a local daemon, the dashboard can only point there — so
 * the sentence that explains them is composed here, per screen, rather than in
 * the desktop crate, which cannot know which screen will show it.
 */
export interface OfferedRemedies {
  replaceDaemon?: boolean;
  /** PRD #1487 D9 — Upgrade, on a remote deck whose daemon is older than this app. */
  upgrade?: boolean;
  connectAnyway?: boolean;
  openDaemons?: boolean;
  reconnect?: boolean;
}

/**
 * What a user can do about an incompatible daemon, naming each button the
 * screen renders and what it does (CLAUDE.md rule 21). The crate's message has
 * already said which side is older and what that means; this is the part that
 * depends on the screen.
 */
export function incompatibleRemedy(connection: ConnectionView, offered: OfferedRemedies): string {
  const sentences: string[] = [];
  if (offered.upgrade) {
    sentences.push("Upgrade installs this app's version on that machine and restarts its daemon onto it; if agents are running there, you are asked before any is stopped.");
  }
  if (offered.replaceDaemon) {
    const count = connection.runningAgentCount;
    sentences.push(count !== undefined && count > 0
      ? `Replace daemon stops this daemon and starts the one that came with this app; ${count} ${count === 1 ? "agent is" : "agents are"} running on it, and you are shown which before any is stopped.`
      : "Replace daemon stops this daemon and starts the one that came with this app.");
  }
  if (offered.connectAnyway) {
    sentences.push("Connect anyway uses this daemon as it is until you quit the app, though some of what it shows may be wrong.");
  }
  if (offered.openDaemons) {
    sentences.push("Open daemons goes to the Daemons screen, where a daemon on this machine can be replaced.");
  }
  if (offered.reconnect) {
    sentences.push("Reconnect tries again once the app and the daemon are on the same version.");
  }
  return sentences.join(" ");
}
