import { useState } from "react";

import type { DeckRuntimeState } from "../types";
import type { SelfUpgradeApi, SelfUpgradeCheck } from "../lib/selfUpgrade";
import { SelfUpgradeDialog } from "./SelfUpgradeDialog";
import { UpgradeDialog, type UpgradeTarget } from "./UpgradeDialog";

/** What the Local daemon section's Upgrade covers: at least one of the two. */
export interface LocalUpgradeRequest {
  /** The local daemon's restart, when it is older than the app. */
  daemon?: UpgradeTarget;
  /** The CLI's check and the calls that upgrade it, when the CLI is behind. */
  cli?: { check: SelfUpgradeCheck; api: SelfUpgradeApi };
}

/** What the CLI's dialog says when the local daemon is not restarted after it. */
export const CLI_LEAVES_DAEMON = "Upgrading the CLI does not restart the daemon on this machine, so its agents keep running.";

/** What the CLI's dialog says when the local daemon is restarted after it: the offer's versions, when the crate gave them. */
export function restartAfterCli(daemon: UpgradeTarget): string {
  const offer = daemon.offer?.kind === "offered" ? daemon.offer : undefined;
  const versions = offer ? `, which runs ${offer.from}, onto ${offer.to},` : " onto";
  return `After the CLI, Agent Deck restarts the daemon on this machine${versions} the version that came with this app. If agents or orchestration roles are running on it, you are shown each one and asked first; Keep current daemon leaves them running.`;
}

/**
 * Issue #1753 — **Upgrade** on the Dashboard's Local daemon section, which
 * covers both things on this machine that can fall behind the app: the
 * `dot-agent-deck` CLI on `PATH`, and the local daemon. Remote sections cover
 * both with one Upgrade because a remote daemon runs from its host's CLI; here
 * the daemon runs the app's own build, so the two are separate steps of one
 * flow.
 *
 * 1. When the CLI is behind, the CLI's plan, in the crate's words, through the
 *    same dialog the app's own upgrade uses, and its own install method. Its
 *    Cancel before anything ran ends the whole flow, having changed nothing.
 * 2. When the daemon runs an older release than the app, the daemon's
 *    restart onto the app's build: the Upgrade dialog's local flow, with its
 *    question naming every agent and role a restart would stop, defaulting to
 *    Keep current daemon. It starts at once when the user has just upgraded
 *    the CLI, which they did knowing this step follows (the CLI dialog says
 *    so); otherwise, after a CLI that could not be upgraded from here, it asks
 *    first, as Upgrade on its own does.
 */
export function LocalUpgradeFlow({ daemon, cli, runtime, onClose }: LocalUpgradeRequest & {
  runtime: Pick<DeckRuntimeState, "upgradeDaemon" | "decideUpgrade">;
  onClose: () => void;
}) {
  const [step, setStep] = useState<{ step: "cli" } | { step: "daemon"; autoStart: boolean }>(cli ? { step: "cli" } : { step: "daemon", autoStart: false });
  if (step.step === "cli" && cli) {
    return (
      <SelfUpgradeDialog
        check={cli.check}
        api={cli.api}
        copies={["cli"]}
        then={daemon ? restartAfterCli(daemon) : CLI_LEAVES_DAEMON}
        onClose={(closed) => {
          if (closed && daemon) setStep({ step: "daemon", autoStart: closed.upgraded });
          else onClose();
        }}
      />
    );
  }
  return daemon ? <UpgradeDialog target={daemon} runtime={runtime} onClose={onClose} autoStart={step.step === "daemon" && step.autoStart} /> : null;
}
