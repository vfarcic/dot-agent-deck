/**
 * Issue #1635 — the desktop's half of upgrading this machine's own copies of
 * Agent Deck: the app, and the `dot-agent-deck` CLI installed beside it.
 *
 * Nothing here decides anything. How each copy was installed, what can be done
 * for it and every sentence the user reads come from the root crate's
 * `self_upgrade` module through `desktop/src-tauri/src/self_upgrade.rs` — the
 * same words `dot-agent-deck upgrade` and the TUI print (CLAUDE.md rule 22).
 * This module carries the shapes, the calls, and which step the dialog is on.
 *
 * Not to be confused with `upgrade.ts`, which upgrades a DAEMON (a remote
 * deck's, or the local one onto this app's version).
 */

/** Which copy a plan or a result is about. */
export type SelfCopy = "app" | "cli";

/**
 * One line of a plan or a result. `text` is the display copy (sanitised and
 * possibly shortened by the bridge). `command`, on a line that is a command
 * for the user to run, is that command exactly as the crate built it, never
 * shortened: it is what Copy writes, and Copy is offered only when it is
 * unchanged by the display sanitiser.
 */
export interface SelfUpgradeLine {
  text: string;
  command: string | null;
}

/** Whether build provenance will be checked for a plan, said before Upgrade. */
export interface SelfUpgradeProvenance {
  checked: boolean;
  /** Why not, when it will not be. */
  reason: string | null;
}

/** The crate's `PlanAction`, kebab-case. */
export type SelfUpgradeAction =
  | "up-to-date"
  | "notify-only"
  | "show-command"
  | "brew-upgrade"
  | "replace-binary"
  | "staged-install"
  | "install-deb"
  | "swap-app"
  | "manual-download";

/** `PlanDto`: one copy's plan. */
export interface SelfUpgradePlan {
  copy: SelfCopy;
  label: string;
  headline: string;
  current: string;
  latest: string;
  action: SelfUpgradeAction;
  /** Whether Upgrade can carry it out. */
  actionable: boolean;
  /** The question the Upgrade button answers; null when there is nothing to confirm. */
  confirmQuestion: string | null;
  provenance: SelfUpgradeProvenance;
  lines: SelfUpgradeLine[];
}

/** `CheckDto`: `desktop_self_upgrade_check`'s answer. */
export interface SelfUpgradeCheck {
  latest: string;
  /** Whether any copy is behind the latest release — the notice shows only then. */
  updateAvailable: boolean;
  /** The notice's text, the same words as the TUI's badge. */
  notice: string | null;
  app: SelfUpgradePlan;
  /** The CLI's own plan, when one is installed beside the app. */
  cli: SelfUpgradePlan | null;
  /** When to ask again: the crate's `UPDATE_RECHECK_INTERVAL`. */
  recheckAfterSecs: number;
}

/** `RunDto`: what an upgrade did, in the crate's words. */
export interface SelfUpgradeResult {
  copy: SelfCopy;
  ok: boolean;
  lines: SelfUpgradeLine[];
  /** The app bundle was replaced, so Relaunch runs the new one. */
  relaunch: boolean;
}

/** What the notice and the dialog call. Injected, so tests drive it. */
export interface SelfUpgradeApi {
  /** Ask for the latest release and plan both copies. Rejects when no check could be made. */
  check(): Promise<SelfUpgradeCheck>;
  /** Carry out the plan for `copy` that the last check returned. */
  run(copy: SelfCopy): Promise<SelfUpgradeResult>;
  /** Restart the app onto the bundle an upgrade put in place. */
  relaunch(): Promise<void>;
}

/**
 * How long to wait before asking again when a check could not be made, and
 * before the first answer says otherwise. The crate's `UPDATE_RECHECK_INTERVAL`
 * (six hours); every successful check carries the real value.
 */
export const FALLBACK_RECHECK_SECS = 6 * 60 * 60;

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const core = await import("@tauri-apps/api/core");
  return core.invoke<T>(command, args);
}

/** The app's own bridge, or `undefined` outside the app (the browser tier, `vite` preview). */
export function tauriSelfUpgradeApi(): SelfUpgradeApi | undefined {
  if (!window.__TAURI_INTERNALS__) return undefined;
  return {
    check: () => invoke<SelfUpgradeCheck>("desktop_self_upgrade_check"),
    run: (copy) => invoke<SelfUpgradeResult>("desktop_self_upgrade_run", { copy }),
    relaunch: () => invoke<void>("desktop_self_upgrade_relaunch"),
  };
}

/** The plans the dialog shows, app first. */
export function plansOf(check: SelfUpgradeCheck): SelfUpgradePlan[] {
  return check.cli ? [check.app, check.cli] : [check.app];
}

/**
 * The copy to offer next: the first actionable plan after `done`, app before
 * CLI. The app goes first because its result decides whether Relaunch is
 * offered at the end.
 */
export function nextOffer(check: SelfUpgradeCheck, done: readonly SelfCopy[]): SelfUpgradePlan | undefined {
  return plansOf(check).find((plan) => plan.actionable && !done.includes(plan.copy));
}
