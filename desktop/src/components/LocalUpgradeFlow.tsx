import { useState } from "react";

import type { DeckRuntimeState } from "../types";
import { cliBehind, type SelfCopy, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";
import { SelfUpgradeDialog } from "./SelfUpgradeDialog";
import { UpgradeDialog, type UpgradeTarget } from "./UpgradeDialog";

/**
 * Where the upgrade was started: the app's own notice (the Dashboard banner's
 * Upgrade… or the rail's upgrade button), or the Local daemon section's
 * Upgrade. Both run this one flow (issue #2062); they differ in whether the
 * app's plan is shown when the app cannot be upgraded from here, and in how
 * the sentence about the app coming first is worded.
 */
export type UpgradeEntry = "notice" | "local";

/** What an upgrade of this machine covers: this machine's copies, the local daemon, or both. */
export interface LocalUpgradeRequest {
  /** The local daemon's restart, when it is older than the app. */
  daemon?: UpgradeTarget;
  /** The check of this machine's copies (the app, and the CLI beside it) and the calls that upgrade them. */
  copies?: { check: SelfUpgradeCheck; api: SelfUpgradeApi };
  /** Where it was started; the Local daemon section when omitted. */
  entry?: UpgradeEntry;
}

/** What the CLI's dialog says when the local daemon is not restarted after it. */
export const CLI_LEAVES_DAEMON = "Upgrading the CLI does not restart the daemon on this machine, so its agents keep running.";

/** What the CLI's dialog says when the local daemon is restarted after it: the offer's versions, when the crate gave them. */
export function restartAfterCli(daemon: UpgradeTarget): string {
  const offer = daemon.offer?.kind === "offered" ? daemon.offer : undefined;
  const versions = offer ? `, which runs ${offer.from}, onto ${offer.to},` : " onto";
  return `After the CLI, Agent Deck restarts the daemon on this machine${versions} the version that came with this app. If agents or orchestration roles are running on it, you are shown each one and asked first; Keep current daemon leaves them running.`;
}

/** What the dialog says when the app itself is behind too, or was upgraded and waits for a relaunch (issue #1959). */
export const APP_BEFORE_CLI = "Agent Deck itself is behind too, so it comes first, then the CLI, to keep the app, the CLI and the daemon on one release. Once Agent Deck is upgraded, relaunch it: it then restarts an older daemon here, asking first if agents run.";

/** {@link APP_BEFORE_CLI} as the app's notice says it: there the user asked about the app, and the CLI is what is behind too (issue #2062). */
export const CLI_AFTER_APP = "The dot-agent-deck CLI here is behind too, so it follows Agent Deck, to keep the app, the CLI and the daemon on one release. Once Agent Deck is upgraded, relaunch it: it then restarts an older daemon here, asking first if agents run.";

/**
 * Whether the Local daemon section's Upgrade leads with the app: it is behind
 * and this app can upgrade it, or it is installed and waiting for a relaunch.
 * An app it cannot upgrade (a folder you cannot write, an unsigned app) is
 * left to the app's own notice, whose dialog says what to do instead.
 */
export function appFirst(check: SelfUpgradeCheck): boolean {
  return check.installed !== null || (check.updateAvailable && check.app.actionable);
}

/**
 * The copies an upgrade started from `entry` shows and offers, app first.
 *
 * - The app's notice always shows the app, which is what it announced, even
 *   when the app cannot be upgraded from here: its plan says what to do.
 * - The Local daemon section shows the app only when {@link appFirst}.
 * - Both show the CLI whenever it is behind (issue #2062), whatever its
 *   install method: one the app can upgrade is offered behind its own
 *   Upgrade, and one it cannot (Nix, a source build, a file another package
 *   owns, a folder with no password prompt to raise) shows what to do instead.
 *   No CLI beside the app — only the copy bundled in it — means no CLI step.
 */
export function upgradeCopies(check: SelfUpgradeCheck, entry: UpgradeEntry): SelfCopy[] {
  const app = entry === "notice" || appFirst(check);
  return [...(app ? ["app" as const] : []), ...(cliBehind(check) ? ["cli" as const] : [])];
}

/** What the dialog says after the plans, about what follows them: nothing to say unless the CLI is covered. */
export function afterCopies(check: SelfUpgradeCheck, copies: readonly SelfCopy[], daemon: UpgradeTarget | undefined, entry: UpgradeEntry): string | undefined {
  if (!copies.includes("cli")) return undefined;
  if (copies.includes("app") && appFirst(check)) return entry === "notice" ? CLI_AFTER_APP : APP_BEFORE_CLI;
  return daemon ? restartAfterCli(daemon) : CLI_LEAVES_DAEMON;
}

/**
 * Issue #1753 — upgrading this machine: the `dot-agent-deck` CLI on `PATH`,
 * and the local daemon. Remote sections cover both with one Upgrade because a
 * remote daemon runs from its host's CLI; here the daemon runs the app's own
 * build, so the two are separate steps of one flow.
 *
 * Issue #2062 — every upgrade entry point runs it: the Local daemon section's
 * Upgrade, and the app's notice (the Dashboard banner's Upgrade… and the
 * rail's upgrade button), so upgrading the app from its notice does not leave
 * a CLI that the app can upgrade for the user to upgrade by hand.
 *
 * 1. The copies' plans ({@link upgradeCopies}), in the crate's words, through
 *    one dialog, each copy behind its own Upgrade and its own install method.
 *    Cancel before anything ran ends the whole flow, having changed nothing.
 * 2. When the CLI was covered and the daemon runs an older release than the
 *    app, the daemon's restart onto the app's build: the Upgrade dialog's
 *    local flow, with its question naming every agent and role a restart
 *    would stop, defaulting to Keep current daemon. It starts at once when the
 *    user has just installed the CLI, which they did knowing this step follows
 *    (the dialog said so, {@link restartAfterCli}); otherwise — a CLI that
 *    could not be upgraded from here, a failed upgrade, a download whose
 *    install command is still to run, or an app that led and was not replaced,
 *    where the dialog said the relaunched app restarts the daemon — it asks
 *    first, as Upgrade on its own does.
 *
 * Issue #1959 — when the app itself is behind, or was upgraded and still runs
 * the old build, the dialog covers the app as well, app first, and says why
 * ({@link APP_BEFORE_CLI}). An app that was replaced ends the flow at its
 * Relaunch: the daemon is restarted by the relaunched app, onto the new
 * release, not here onto the build this app still runs.
 */
export function LocalUpgradeFlow({ daemon, copies, entry = "local", runtime, onClose }: LocalUpgradeRequest & {
  runtime: Pick<DeckRuntimeState, "upgradeDaemon" | "decideUpgrade">;
  onClose: () => void;
}) {
  /* What the dialog covers, fixed when the flow opens, as the dialog keeps the check it opened with. */
  const [covered] = useState(() => (copies ? upgradeCopies(copies.check, entry) : []));
  const [step, setStep] = useState<{ step: "copies" } | { step: "daemon"; autoStart: boolean }>(covered.length > 0 ? { step: "copies" } : { step: "daemon", autoStart: false });
  if (step.step === "copies" && copies) {
    /* The daemon follows only an upgrade that covered the CLI: the app alone is followed by its relaunch. */
    const next = covered.includes("cli") ? daemon : undefined;
    /* Whether the dialog said the daemon is restarted next, rather than by the relaunched app (Qodo, PR #2063). */
    const restartSaid = !(covered.includes("app") && appFirst(copies.check));
    return (
      <SelfUpgradeDialog
        check={copies.check}
        api={copies.api}
        copies={covered}
        then={afterCopies(copies.check, covered, daemon, entry)}
        onClose={(closed) => {
          if (closed && next && !closed.relaunch) setStep({ step: "daemon", autoStart: closed.installed && restartSaid });
          else onClose();
        }}
      />
    );
  }
  return daemon ? <UpgradeDialog target={daemon} runtime={runtime} onClose={onClose} autoStart={step.step === "daemon" && step.autoStart} /> : null;
}
