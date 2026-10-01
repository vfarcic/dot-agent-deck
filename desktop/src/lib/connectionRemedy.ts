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
  connectAnyway?: boolean;
  openDaemons?: boolean;
  reconnect?: boolean;
  /**
   * The screen COULD offer Replace daemon for this daemon (a live, local deck)
   * but withholds it because agents are running or their count is unknown.
   * Said rather than left as a missing button.
   */
  replaceWithheld?: boolean;
}

/**
 * What a user can do about an incompatible daemon, naming each button the
 * screen renders and what it does (CLAUDE.md rule 21). The crate's message has
 * already said which side is older and what that means; this is the part that
 * depends on the screen.
 */
export function incompatibleRemedy(connection: ConnectionView, offered: OfferedRemedies): string {
  const sentences: string[] = [];
  if (offered.replaceDaemon) {
    sentences.push("Replace daemon stops this daemon and starts the one that came with this app.");
  } else if (offered.replaceWithheld) {
    const count = connection.runningAgentCount;
    sentences.push(count === undefined
      ? "Replace daemon is not offered because this daemon did not say how many agents it is running."
      : `Replace daemon is not offered while ${count} ${count === 1 ? "agent is" : "agents are"} running on this daemon; close ${count === 1 ? "it" : "them"} first.`);
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
