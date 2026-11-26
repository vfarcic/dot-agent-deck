/**
 * Issue #1600 — the desktop's half of **Upgrade all**: the shapes the desktop
 * crate sends, whether to offer the button, and the dialog's own phrases.
 *
 * Nothing here decides anything about the batch. Which decks are upgraded,
 * what stops it and what each deck's line says come from the root crate's
 * `daemon_upgrade_batch`, the batch the CLI's `remote upgrade --all` and the
 * TUI's Upgrade all daemons dialog run too; every line the dialog shows for a
 * deck arrives here already written. The phrases below are the dialog's own,
 * spelled as the TUI's dialog spells them (`src/upgrade_all_dialog.rs`,
 * CLAUDE.md rule 22).
 */
import type { ConnectionView } from "../types";
import type { UpgradeDecisionEvent, UpgradeStage } from "./upgrade";
import { upgradeOffered } from "./upgrade";

/** How a deck reads in the plan. */
type PlannedKind = "upgrade" | "skip" | "unreachable";

export interface PlannedDeck {
  deck: string;
  deckId?: string;
  kind: PlannedKind;
  /** What will happen to it, in the crate's words. */
  line: string;
}

/** `desktop_upgrade_all_check`'s answer. */
export interface UpgradeAllPlan {
  batchId: string;
  /** "Checked 3 remote decks: 2 to upgrade." */
  headline: string;
  toUpgrade: number;
  decks: PlannedDeck[];
}

/** How a deck's part of the batch ended. */
export type DeckStatus = "upgraded" | "kept" | "skipped" | "failed";

export interface DeckReport {
  deck: string;
  deckId?: string;
  status: DeckStatus;
  /** The result, in the crate's words. */
  line: string;
  /** Whether the deck's terminal sessions ended with its old daemon. */
  endsSessions: boolean;
}

/** `desktop_upgrade_all_run`'s answer. */
export interface UpgradeAllReport {
  /** "Upgrade all: 1 upgraded, 1 skipped of 2 remote decks." */
  headline: string;
  /** A deck failed or could not be checked. */
  failed: boolean;
  decks: DeckReport[];
}

/** `desktop://upgrade-all`, for the deck at `index` in the plan. */
type UpgradeAllDeckEvent =
  | { type: "started"; index: number }
  | { type: "progress"; index: number; stage: UpgradeStage }
  | { type: "finished"; index: number; report: DeckReport };

/** What a run's listener hears: its decks' events, and each daemon's question. */
export type UpgradeAllEvent =
  | UpgradeAllDeckEvent
  | ({ type: "decision" } & UpgradeDecisionEvent);

/** The dialog's title, the TUI's and the help entry's words too. */
export const UPGRADE_ALL_TITLE = "Upgrade all daemons";
/** What the dialog shows while every deck is checked. */
export const UPGRADE_ALL_CHECKING = "Checking every remote deck on the deck list…";
/** The plan's question, before anything runs. */
export function upgradeAllQuestion(toUpgrade: number): string {
  return `Upgrade ${toUpgrade} ${toUpgrade === 1 ? "daemon" : "daemons"}? Each daemon with agents or orchestration roles running asks before it stops any.`;
}
export const UPGRADE_ALL_NOTHING = "Nothing to upgrade.";
export const UPGRADE_ALL_RUNNING = "Upgrading, one daemon at a time…";
export const UPGRADE_ALL_STOPPING = "Stopping after this daemon…";
export const UPGRADE_ALL_WAITING = "Waiting to be upgraded.";
/** The button that starts no deck after the current one. */
export const UPGRADE_ALL_STOP = "Stop after this daemon";
/** What voice's "close" answers while the batch runs. */
export const UPGRADE_ALL_RUNNING_CLOSE_BLOCKED = "Upgrade all is still running. The dialog can be closed once every daemon has its result.";

/** A deck being upgraded, at `stage`: "Upgrading: restarting the daemon…". */
export function upgradingLine(stage: UpgradeStage | undefined): string {
  const doing = stage === "restarting" ? "restarting the daemon" : stage === "verifying" ? "checking the new daemon answers" : "installing the new version";
  return `Upgrading: ${doing}…`;
}

/**
 * Whether the Dashboard offers **Upgrade all**: at least one remote deck the
 * crate says runs an older daemon than this app (PRD #1487 D8, the rule each
 * deck's own Upgrade follows). The batch checks every deck again when it opens.
 */
export function upgradeAllOffered(connections: readonly ConnectionView[]): boolean {
  return connections.some((connection) => connection.deckKind === "remote" && upgradeOffered(connection));
}
