/**
 * PRD #1487 M5 — the desktop's half of upgrading a daemon: the shapes the
 * desktop crate sends, whether to offer the action, and what the user reads.
 *
 * Nothing here decides anything about an upgrade. Whether a daemon is older
 * than the app is worked out in Rust (`daemon_upgrade::upgrade_offer`) and
 * arrives as `connection.upgradeOffer`; what happens to running agents is the
 * daemon's policy, asked through the decision event. This module only renders.
 */
import type { ConnectionView } from "../types";

/** `daemon_upgrade::UpgradeOffer`, as each deck's connection carries it. */
export type UpgradeOffer =
  | { kind: "offered"; from: string; to: string }
  | { kind: "current" }
  | { kind: "daemon-newer"; daemon: string }
  | { kind: "unknown" };

export type UpgradeStage = "installing" | "restarting" | "verifying";

/** One agent a restart would stop (or stopped). */
export interface UpgradeStopAgent {
  id: string;
  label: string;
  paneId?: string;
  cwd?: string;
}

/** One orchestration role a restart would stop (or stopped). */
export interface UpgradeStopRole {
  paneId: string;
  role: string;
  orchestration: string;
  isOrchestrator: boolean;
}

export interface UpgradeStopSet {
  agents: UpgradeStopAgent[];
  roles: UpgradeStopRole[];
}

export type NotRestartedReason =
  | { kind: "kept-by-user"; atStake: UpgradeStopSet }
  | { kind: "no-one-to-ask"; atStake: UpgradeStopSet }
  | { kind: "stale-confirmation"; atStake: UpgradeStopSet }
  | { kind: "another-restart-in-progress" }
  | { kind: "no-daemon-running" }
  | { kind: "installed-build-too-old" };

/** `desktop_upgrade_daemon`'s answer — every arm of `UpgradeOutcome`. */
export type UpgradeOutcome =
  | { outcome: "restarted"; fromVersion: string; toVersion: string; stopped: UpgradeStopSet }
  | { outcome: "installed-not-restarted"; fromVersion?: string; installedVersion: string; reason: NotRestartedReason }
  | { outcome: "installed-daemon-too-old"; installedVersion: string; daemonVersion?: string; remedy: string }
  | { outcome: "failed"; stage: UpgradeStage; reason: string; installedVersion?: string };

/** `desktop://upgrade-progress`. */
export interface UpgradeProgressEvent {
  deckId: string;
  upgradeId: string;
  progress: { stage: UpgradeStage; detail?: string | null };
}

/** `desktop://upgrade-decision`: "restarting stops these — restart now?". */
export interface UpgradeDecisionEvent {
  deckId: string;
  upgradeId: string;
  atStake: UpgradeStopSet;
  /** What would stop changed since the last time this upgrade asked. */
  stale: boolean;
}

export type UpgradeEvent =
  | ({ type: "progress" } & UpgradeProgressEvent)
  | ({ type: "decision" } & UpgradeDecisionEvent);

export type UpgradeChoice = "restart-now" | "keep-current";

/**
 * Which flow the dialog runs: **Upgrade** on a remote deck installs this app's
 * version on that machine; **Replace daemon** on the local deck starts the
 * build that came with this app. One procedure underneath, two ways to say it.
 */
export type UpgradeKind = "upgrade" | "replace";

/**
 * Whether a deck's card, banner or screen shows **Upgrade** (PRD #1487 D8, D9):
 * a remote deck whose daemon the crate says is older than this app. The local
 * deck is never offered Upgrade — its remedy is Replace daemon.
 */
export function upgradeOffered(connection: ConnectionView): connection is ConnectionView & { upgradeOffer: Extract<UpgradeOffer, { kind: "offered" }> } {
  return connection.deckKind === "remote" && connection.upgradeOffer?.kind === "offered";
}

/** The stage list the progress view walks, in order. */
export const UPGRADE_STAGES: readonly UpgradeStage[] = ["installing", "restarting", "verifying"];

/** What each stage is called while it runs. */
export function stageLabel(stage: UpgradeStage, kind: UpgradeKind): string {
  switch (stage) {
    case "installing":
      return kind === "replace" ? "Preparing this app's daemon" : "Installing the new version";
    case "restarting":
      return "Restarting the daemon";
    case "verifying":
      return "Checking the new daemon answers";
  }
}

/** One line per agent and role, for the decision dialog and the outcome. */
export function stopSetLines(set: UpgradeStopSet): string[] {
  const agents = set.agents.map((agent) => {
    const place = [agent.paneId ? `pane ${agent.paneId}` : undefined, agent.cwd ? `in ${agent.cwd}` : undefined].filter(Boolean);
    return place.length ? `Agent ${agent.label} (${place.join(", ")})` : `Agent ${agent.label}`;
  });
  const roles = set.roles.map((role) => `Role ${role.role} of ${role.orchestration}, pane ${role.paneId}${role.isOrchestrator ? " (the orchestrator)" : ""}`);
  return [...agents, ...roles];
}

/** How many things a stop set names, as words: "2 agents and 1 orchestration role". */
export function stopSetCount(set: UpgradeStopSet): string {
  const parts: string[] = [];
  if (set.agents.length) parts.push(`${set.agents.length} ${set.agents.length === 1 ? "agent" : "agents"}`);
  if (set.roles.length) parts.push(`${set.roles.length} orchestration ${set.roles.length === 1 ? "role" : "roles"}`);
  return parts.join(" and ") || "nothing";
}

export type OutcomeTone = "success" | "neutral" | "failure";

/** What the dialog shows when an upgrade finishes. */
export interface OutcomeView {
  tone: OutcomeTone;
  title: string;
  /** Sentences, in reading order. */
  body: string[];
  /** Agents and roles the outcome names (stopped, or still running). */
  list?: string[];
}

/**
 * The outcome in plain words (CLAUDE.md rule 21): what happened, what is
 * running now, and what the user can do — for every arm, so no result renders
 * as a blank dialog.
 */
export function outcomeView(outcome: UpgradeOutcome, deck: string, kind: UpgradeKind): OutcomeView {
  const where = kind === "replace" ? "this machine" : deck;
  switch (outcome.outcome) {
    case "restarted": {
      const stopped = outcome.stopped.agents.length + outcome.stopped.roles.length > 0;
      return {
        tone: "success",
        title: kind === "replace" ? "Daemon replaced" : "Daemon upgraded",
        body: [
          `The daemon on ${where} now runs ${outcome.toVersion} (it was ${outcome.fromVersion}).`,
          stopped ? "These were stopped by the restart:" : "Nothing was running, so nothing was stopped.",
        ],
        ...(stopped ? { list: stopSetLines(outcome.stopped) } : {}),
      };
    }
    case "installed-not-restarted": {
      const installed = kind === "replace" ? "" : `${outcome.installedVersion} is installed on ${where}. `;
      const keeps = outcome.fromVersion ? `The daemon keeps running ${outcome.fromVersion}` : "The daemon keeps running";
      const reason = outcome.reason;
      switch (reason.kind) {
        case "kept-by-user":
          return {
            tone: "neutral",
            title: "Daemon kept running",
            body: [`${installed}${keeps}, as you chose, so these keep running:`, kind === "replace" ? "Press Replace daemon again when they have finished." : "It switches to the new version the next time it restarts. Press Upgrade again when they have finished."],
            list: stopSetLines(reason.atStake),
          };
        case "no-one-to-ask":
          return {
            tone: "neutral",
            title: "Daemon kept running",
            body: [`${installed}${keeps}, because nobody confirmed stopping these:`, "Try again and choose Restart now, or wait until they have finished."],
            list: stopSetLines(reason.atStake),
          };
        case "stale-confirmation":
          return {
            tone: "neutral",
            title: "Daemon kept running",
            body: [`${installed}${keeps}, because what was running kept changing while you were asked. Running now:`, "Try again once it settles."],
            list: stopSetLines(reason.atStake),
          };
        case "another-restart-in-progress":
          return {
            tone: "neutral",
            title: "Another restart is already running",
            body: [`${installed}Someone else is already restarting this daemon, so it was not restarted from here. Reconnect in a moment to see the result.`],
          };
        case "no-daemon-running":
          return {
            tone: "neutral",
            title: kind === "replace" ? "No daemon was running" : "Installed — no daemon was running",
            body: [kind === "replace" ? "There was no daemon to replace. Start daemon starts the one that came with this app." : `${installed}No daemon was running there, so nothing was restarted; the next one to start runs the new version.`],
          };
        case "installed-build-too-old":
          return {
            tone: "neutral",
            title: "Installed — the daemon was not restarted",
            body: [
              `${installed}That version is too old to restart the daemon from this app, so ${keeps.charAt(0).toLowerCase()}${keeps.slice(1)}.`,
              `To switch to ${outcome.installedVersion}, run \`dot-agent-deck connect ${deck}\` in a terminal: the TUI on that machine restarts the daemon onto it, asking first when agents are running.`,
            ],
          };
      }
      break;
    }
    case "installed-daemon-too-old":
      return {
        tone: "neutral",
        title: "Installed — the daemon could not restart itself",
        body: [
          `${outcome.installedVersion} is installed on ${where}, but the daemon running there${outcome.daemonVersion ? ` (${outcome.daemonVersion})` : ""} is too old to be restarted from this app, so it keeps running the old version.`,
          outcome.remedy,
        ],
      };
    case "failed": {
      const doing = outcome.stage === "installing"
        ? (kind === "replace" ? "preparing this app's daemon" : "installing the new version")
        : outcome.stage === "restarting" ? "restarting the daemon" : "checking the restarted daemon";
      const after = outcome.stage === "verifying"
        ? "The old daemon was asked to restart; Reconnect shows whatever is answering now."
        : outcome.installedVersion && kind !== "replace"
          ? `${outcome.installedVersion} is installed; the daemon that was running keeps running.`
          : "Nothing was changed; the daemon that was running keeps running.";
      return {
        tone: "failure",
        title: kind === "replace" ? "Replace daemon failed" : "Upgrade failed",
        body: [`It failed while ${doing}: ${outcome.reason}`, after],
      };
    }
  }
  // Unreachable for a well-formed outcome; an unknown arm from a newer crate
  // still says something true rather than rendering nothing.
  return { tone: "neutral", title: "Upgrade finished", body: ["Reconnect to see what the daemon is running now."] };
}
