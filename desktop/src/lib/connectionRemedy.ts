import type { ConnectionView } from "../types";

/** The Connect anyway confirmation, shared by the Daemons screen and the dashboard so the two cannot drift. */
export const CONNECT_ANYWAY_BODY = "This daemon and this app are different versions, so this app may show some of this daemon's information wrongly. Agent Deck will connect and keep a warning on screen until you quit the app; nothing is remembered after that.";

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
