import { describe, expect, it } from "vitest";

import type { ConnectionView } from "../types";
import { incompatibleRemedy } from "./connectionRemedy";
import { localUpgradeAtStart, outcomeView, stageLabel, stopSetCount, stopSetLines, upgradeEndedDeckSessions, upgradeKindOf, upgradeOffered, type UpgradeOffer, type UpgradeOutcome, type UpgradeStopSet } from "./upgrade";

const AT_STAKE: UpgradeStopSet = {
  agents: [
    { id: "1", label: "coder", paneId: "4", cwd: "/work/app" },
    { id: "2", label: "scratch" },
  ],
  roles: [{ paneId: "3", role: "orchestrator", orchestration: "tdd", isOrchestrator: true }],
};

const connection = (deckKind: "local" | "remote", upgradeOffer?: UpgradeOffer, status: ConnectionView["status"] = "connected"): ConnectionView => ({
  status,
  socketPath: "dev@build-box",
  deckKind,
  ...(upgradeOffer ? { upgradeOffer } : {}),
});

describe("upgradeOffered (PRD #1487 D8)", () => {
  /**
   * The visibility table. The crate decides older/current/newer/unknown; the
   * webview only reads its answer, and offers Upgrade for exactly one cell.
   */
  it.each([
    ["remote", { kind: "offered", from: "0.44.0", to: "0.45.0" }, true],
    ["remote", { kind: "current" }, false],
    ["remote", { kind: "daemon-newer", daemon: "0.46.0" }, false],
    ["remote", { kind: "unknown" }, false],
    ["remote", undefined, false],
    // Issue #1636: the local deck is offered it too, while it is connected.
    ["local", { kind: "offered", from: "0.44.0", to: "0.45.0" }, true],
    ["local", { kind: "current" }, false],
    ["local", { kind: "daemon-newer", daemon: "0.46.0" }, false],
    ["local", { kind: "unknown" }, false],
  ] as const)("a %s deck with offer %j is offered: %s", (deckKind, offer, offered) => {
    expect(upgradeOffered(connection(deckKind, offer))).toBe(offered);
  });

  it("offers a refused local daemon Replace daemon rather than Upgrade, and a refused remote one Upgrade", () => {
    const older = { kind: "offered", from: "0.44.0", to: "0.45.0" } as const;
    expect(upgradeOffered(connection("local", older, "error"))).toBe(false);
    expect(upgradeOffered(connection("remote", older, "error"))).toBe(true);
  });

  it("upgrades a remote deck by installing and the local deck by restarting onto this app's build", () => {
    expect(upgradeKindOf({ deckKind: "remote" })).toBe("upgrade");
    expect(upgradeKindOf({ deckKind: "local" })).toBe("local-upgrade");
  });
});

describe("localUpgradeAtStart (issue #1636)", () => {
  const older = { kind: "offered", from: "0.44.0", to: "0.45.0" } as const;
  const deck = (connection: ConnectionView) => ({ connection });
  const local = (overrides: Partial<ConnectionView> = {}): ConnectionView => ({ ...connection("local", older), deckId: "local:/run/attach.sock", daemonBuildVersion: "0.44.0-gabc1234", ...overrides });

  it("picks the connected local deck whose daemon is older, keyed by its version", () => {
    const found = localUpgradeAtStart([deck({ ...connection("remote", older), deckId: "ssh:build-box" }), deck(local())]);
    expect(found?.connection.deckId).toBe("local:/run/attach.sock");
    expect(found?.connection.upgradeOffer).toEqual(older);
    expect(found?.key).toBe(localUpgradeAtStart([deck(local())])?.key);
  });

  it("leaves remote decks, current, newer, refused and unidentified local daemons alone", () => {
    expect(localUpgradeAtStart([deck({ ...connection("remote", older), deckId: "ssh:build-box" })])).toBeUndefined();
    expect(localUpgradeAtStart([deck(local({ upgradeOffer: { kind: "current" } }))])).toBeUndefined();
    expect(localUpgradeAtStart([deck(local({ upgradeOffer: { kind: "daemon-newer", daemon: "0.46.0" } }))])).toBeUndefined();
    expect(localUpgradeAtStart([deck(local({ status: "error" }))])).toBeUndefined();
    expect(localUpgradeAtStart([deck(local({ deckId: undefined }))])).toBeUndefined();
    expect(localUpgradeAtStart([])).toBeUndefined();
  });

  it("gives another daemon version another key, so it is upgraded on its own", () => {
    const first = localUpgradeAtStart([deck(local())])?.key;
    expect(localUpgradeAtStart([deck(local({ daemonBuildVersion: "0.44.0-gdef5678" }))])?.key).not.toBe(first);
    expect(localUpgradeAtStart([deck(local({ upgradeOffer: { kind: "offered", from: "0.43.0", to: "0.45.0" } }))])?.key).not.toBe(first);
  });
});

describe("the restart question's list", () => {
  it("names every agent and every orchestration role", () => {
    expect(stopSetLines(AT_STAKE)).toEqual([
      "Agent coder (pane 4, in /work/app)",
      "Agent scratch",
      "Role orchestrator of tdd, pane 3 (the orchestrator)",
    ]);
    expect(stopSetCount(AT_STAKE)).toBe("2 agents and 1 orchestration role");
    expect(stopSetCount({ agents: [AT_STAKE.agents[0]], roles: [] })).toBe("1 agent");
  });
});

describe("stage labels", () => {
  it("say installing for an upgrade and preparing for a Replace", () => {
    expect(stageLabel("installing", "upgrade")).toBe("Installing the new version");
    expect(stageLabel("installing", "replace")).toBe("Preparing this app's daemon");
    expect(stageLabel("installing", "local-upgrade")).toBe("Preparing this app's daemon");
    expect(stageLabel("restarting", "upgrade")).toBe("Restarting the daemon");
    expect(stageLabel("verifying", "replace")).toBe("Checking the new daemon answers");
  });
});

describe("outcomeView (CLAUDE.md rule 21)", () => {
  const outcomes: [string, UpgradeOutcome][] = [
    ["restarted, idle", { outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: { agents: [], roles: [] } }],
    ["restarted, stopping agents", { outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: AT_STAKE }],
    ["kept by the user", { outcome: "installed-not-restarted", fromVersion: "0.44.0", installedVersion: "0.45.0", reason: { kind: "kept-by-user", atStake: AT_STAKE } }],
    ["no one to ask", { outcome: "installed-not-restarted", fromVersion: "0.44.0", installedVersion: "0.45.0", reason: { kind: "no-one-to-ask", atStake: AT_STAKE } }],
    ["stale confirmation", { outcome: "installed-not-restarted", installedVersion: "0.45.0", reason: { kind: "stale-confirmation", atStake: AT_STAKE } }],
    ["another restart running", { outcome: "installed-not-restarted", installedVersion: "0.45.0", reason: { kind: "another-restart-in-progress" } }],
    ["no daemon running", { outcome: "installed-not-restarted", installedVersion: "0.45.0", reason: { kind: "no-daemon-running" } }],
    ["daemon too old", { outcome: "installed-daemon-too-old", installedVersion: "0.45.0", daemonVersion: "0.30.0", remedy: "Connect with `dot-agent-deck connect build-box` and accept its restart prompt." }],
    ["failed installing", { outcome: "failed", stage: "installing", reason: "ssh: connection timed out" }],
    ["failed restarting", { outcome: "failed", stage: "restarting", reason: "the installed build did not answer", installedVersion: "0.45.0" }],
    ["failed verifying", { outcome: "failed", stage: "verifying", reason: "the new daemon did not answer within 20s", installedVersion: "0.45.0" }],
    ["installed build too old", { outcome: "installed-not-restarted", installedVersion: "0.40.0", reason: { kind: "installed-build-too-old" } }],
    ["older daemon busy", { outcome: "installed-not-restarted", fromVersion: "0.44.0", installedVersion: "0.45.0", reason: { kind: "older-daemon-busy", atStake: AT_STAKE } }],
    ["failed after the build landed", { outcome: "failed", stage: "installing", reason: "0.45.0 was installed, but reinstalling the hooks failed: settings.json is not writable", installedVersion: "0.45.0" }],
    ["stopped unexpectedly", { outcome: "interrupted" }],
  ];

  it.each(outcomes)("%s renders a title and at least one sentence, with no internals", (_name, outcome) => {
    for (const kind of ["upgrade", "local-upgrade", "replace"] as const) {
      const view = outcomeView(outcome, "build-box", kind);
      expect(view.title.length).toBeGreaterThan(0);
      expect(view.body.length).toBeGreaterThan(0);
      expect(view.body.every((sentence) => sentence.trim().length > 0)).toBe(true);
      expect(view.body.join(" ")).not.toMatch(/capability|protocol|RestartDaemon|NeedsConfirmation|ClientSpawns|stop set/i);
    }
  });

  it("says an older installed build left the daemon running, and how to switch", () => {
    const view = outcomeView(outcomes[11][1], "build-box", "upgrade");
    expect(view.tone).toBe("neutral");
    expect(view.body[0]).toBe("0.40.0 is installed on build-box. That version is too old to restart the daemon from this app, so the daemon keeps running.");
    expect(view.body[1]).toContain("run `dot-agent-deck connect build-box` in a terminal");
  });

  it("says what was stopped, and only when something was", () => {
    const idle = outcomeView(outcomes[0][1], "build-box", "upgrade");
    expect(idle.tone).toBe("success");
    expect(idle.body.join(" ")).toContain("The daemon on build-box now runs 0.45.0 (it was 0.44.0).");
    expect(idle.body.join(" ")).toContain("nothing was stopped");
    expect(idle.list).toBeUndefined();
    const busy = outcomeView(outcomes[1][1], "build-box", "upgrade");
    expect(busy.list).toEqual(stopSetLines(AT_STAKE));
  });

  it("names what keeps running when the user kept the daemon, and how to finish later", () => {
    const view = outcomeView(outcomes[2][1], "build-box", "upgrade");
    expect(view.tone).toBe("neutral");
    expect(view.body[0]).toBe("0.45.0 is installed on build-box. The daemon keeps running 0.44.0, as you chose, so these keep running:");
    expect(view.list).toEqual(stopSetLines(AT_STAKE));
    expect(view.body.join(" ")).toContain("Press Upgrade again");
    expect(outcomeView(outcomes[2][1], "this machine", "replace").body.join(" ")).toContain("Press Replace daemon again");
  });

  it("words the local Upgrade as a restart on this machine that installs nothing", () => {
    const restarted = outcomeView(outcomes[0][1], "Local daemon", "local-upgrade");
    expect(restarted.title).toBe("Daemon upgraded");
    expect(restarted.body[0]).toBe("The daemon on this machine now runs 0.45.0 (it was 0.44.0).");
    const kept = outcomeView(outcomes[2][1], "Local daemon", "local-upgrade");
    expect(kept.title).toBe("Daemon kept running");
    expect(kept.body).toEqual(["The daemon keeps running 0.44.0, as you chose, so these keep running:", "Press Upgrade again when they have finished."]);
    expect(kept.list).toEqual(stopSetLines(AT_STAKE));
    const busy = outcomeView(outcomes[12][1], "Local daemon", "local-upgrade");
    expect(busy.body[1]).toBe("Stop them, or let them finish, then press Upgrade again.");
    expect(outcomeView(outcomes[6][1], "Local daemon", "local-upgrade").body).toEqual(["There was no daemon to upgrade. Start daemon starts the one that came with this app."]);
    expect(outcomeView(outcomes[9][1], "Local daemon", "local-upgrade").title).toBe("Upgrade failed");
    // Two arms only an install over SSH produces: the local deck installs
    // nothing, and an older local daemon is replaced rather than asked.
    const remoteOnly = new Set(["daemon too old", "installed build too old"]);
    for (const [name, outcome] of outcomes) {
      if (remoteOnly.has(name)) continue;
      expect(outcomeView(outcome, "Local daemon", "local-upgrade").body.join(" ")).not.toMatch(/is installed|Local daemon/);
    }
  });

  it("names what runs on an older daemon Replace would not stop, and how to finish", () => {
    const view = outcomeView(outcomes[12][1], "this machine", "replace");
    expect(view.tone).toBe("neutral");
    expect(view.title).toBe("Daemon kept running");
    expect(view.body).toEqual([
      "The daemon keeps running 0.44.0. It is too old to restart itself, so it is replaced only when nothing is running on it, and these are running:",
      "Stop them, or let them finish, then press Replace daemon again.",
    ]);
    expect(view.list).toEqual(stopSetLines(AT_STAKE));
  });

  /**
   * Scenario: the upgrade run stopped without an outcome (its task panicked).
   * The text says it stopped unexpectedly and that Reconnect shows what is
   * answering now — never that nothing changed, since it may have installed
   * or restarted first (PRD #1487, Qodo #15).
   */
  it("says a run that stopped unexpectedly may have changed things, and to Reconnect", () => {
    const upgrade = outcomeView({ outcome: "interrupted" }, "build-box", "upgrade");
    expect(upgrade.tone).toBe("failure");
    expect(upgrade.title).toBe("Upgrade stopped unexpectedly");
    expect(upgrade.body).toEqual([
      "The upgrade stopped unexpectedly, so it is not known whether the new version was installed on build-box or the daemon restarted.",
      "Reconnect shows which daemon is answering on build-box now.",
    ]);
    const replace = outcomeView({ outcome: "interrupted" }, "this machine", "replace");
    expect(replace.title).toBe("Replace daemon stopped unexpectedly");
    expect(replace.body).toEqual([
      "Replacing the daemon stopped unexpectedly, so it is not known whether the daemon was replaced.",
      "Reconnect shows which daemon is answering on this machine now.",
    ]);
    for (const view of [upgrade, replace]) expect(view.body.join(" ")).not.toMatch(/nothing was changed/i);
  });

  it("carries the crate's remedy for a daemon too old to restart itself", () => {
    const view = outcomeView(outcomes[7][1], "build-box", "upgrade");
    expect(view.body[0]).toContain("the daemon running there (0.30.0) is too old to be restarted from this app");
    expect(view.body[1]).toContain("dot-agent-deck connect build-box");
  });

  it("says which stage failed and what is still running", () => {
    const installing = outcomeView(outcomes[8][1], "build-box", "upgrade");
    expect(installing.tone).toBe("failure");
    expect(installing.body).toEqual([
      "It failed while installing the new version: ssh: connection timed out",
      "The daemon that was running keeps running.",
    ]);
    expect(outcomeView(outcomes[13][1], "build-box", "upgrade").body).toEqual([
      "It failed while installing the new version: 0.45.0 was installed, but reinstalling the hooks failed: settings.json is not writable",
      "0.45.0 is installed, but the upgrade stopped before restarting the daemon, so the daemon that was running keeps running. Press Upgrade again to finish.",
    ]);
    for (const [, outcome] of outcomes) {
      for (const kind of ["upgrade", "replace"] as const) {
        expect(outcomeView(outcome, "build-box", kind).body.join(" ")).not.toMatch(/nothing was changed/i);
      }
    }
    expect(outcomeView(outcomes[9][1], "build-box", "upgrade").body[1]).toBe("0.45.0 is installed; the daemon that was running keeps running.");
    expect(outcomeView(outcomes[10][1], "build-box", "upgrade").body[0]).toBe("It failed while checking the restarted daemon: the new daemon did not answer within 20s");
  });

  /**
   * Scenario: a restart fails after its reply was lost and the old daemon no
   * longer answers as itself (`oldDaemonGone`). The dialog does not say the
   * old daemon keeps running: it says it may have stopped and that Reconnect
   * shows what answers now. A restart the old daemon refused still says it
   * keeps running (Qodo 4201244671).
   */
  it("does not say a daemon that may have stopped keeps running", () => {
    const restarting = (installedVersion: string | undefined, oldDaemonGone: boolean): UpgradeOutcome => ({
      outcome: "failed",
      stage: "restarting",
      reason: "the restart reply was lost",
      installedVersion,
      oldDaemonGone,
    });
    expect(outcomeView(restarting("0.45.0", true), "build-box", "upgrade").body[1]).toBe(
      "0.45.0 is installed. The daemon that was running may have stopped; Reconnect shows what is answering now.",
    );
    expect(outcomeView(restarting(undefined, true), "build-box", "upgrade").body[1]).toBe(
      "The daemon that was running may have stopped; Reconnect shows what is answering now.",
    );
    expect(outcomeView(restarting(undefined, true), "this machine", "replace").body[1]).toBe(
      "The daemon that was running may have stopped; Reconnect shows what is answering now.",
    );
    for (const kind of ["upgrade", "replace"] as const) {
      expect(outcomeView(restarting("0.45.0", true), "build-box", kind).body.join(" ")).not.toMatch(/keeps running/);
    }
    expect(outcomeView(restarting("0.45.0", false), "build-box", "upgrade").body[1]).toBe(
      "0.45.0 is installed; the daemon that was running keeps running.",
    );
  });

  it("says a replaced binary that failed its version check is in place, not that nothing changed", () => {
    const failed = (installedVersion: string): UpgradeOutcome => ({
      outcome: "failed",
      stage: "installing",
      reason: "~/.local/bin/dot-agent-deck on the remote was replaced, but the new binary did not pass its version check",
      installedVersion,
    });
    expect(outcomeView(failed("an unverified build"), "build-box", "upgrade").body[1]).toBe(
      "An unverified build is installed, but the upgrade stopped before restarting the daemon, so the daemon that was running keeps running. Press Upgrade again to finish.",
    );
    expect(outcomeView(failed("0.44.0"), "build-box", "upgrade").body[1]).toMatch(/^0\.44\.0 is installed, but the upgrade stopped/);
  });
});

describe("upgradeEndedDeckSessions (Qodo 4200693875)", () => {
  /**
   * Scenario: each outcome the crate can return is asked whether the deck's
   * terminal sessions ended with the old daemon. A restart and a failure the
   * crate marks `oldDaemonGone` say yes, and so does a run that stopped
   * unexpectedly; a failure that left the old daemon
   * answering, one from a crate that sent no mark, and every not-restarted
   * outcome say no.
   */
  it.each<[string, UpgradeOutcome, boolean]>([
    ["restarted", { outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: AT_STAKE }, true],
    ["failed verifying, old daemon gone", { outcome: "failed", stage: "verifying", reason: "r", installedVersion: "0.45.0", oldDaemonGone: true }, true],
    ["failed restarting after a lost reply, old daemon gone", { outcome: "failed", stage: "restarting", reason: "r", installedVersion: "0.45.0", oldDaemonGone: true }, true],
    ["failed restarting, old daemon still answering", { outcome: "failed", stage: "restarting", reason: "r", installedVersion: "0.45.0", oldDaemonGone: false }, false],
    ["failed with no mark", { outcome: "failed", stage: "installing", reason: "r" }, false],
    ["not restarted", { outcome: "installed-not-restarted", installedVersion: "0.45.0", reason: { kind: "no-daemon-running" } }, false],
    ["daemon too old", { outcome: "installed-daemon-too-old", installedVersion: "0.45.0", remedy: "r" }, false],
    ["stopped unexpectedly", { outcome: "interrupted" }, true],
  ])("%s", (_, outcome, ended) => {
    expect(upgradeEndedDeckSessions(outcome)).toBe(ended);
  });
});

describe("incompatibleRemedy (PRD #1487 D9, D10)", () => {
  it("names Upgrade beside Connect anyway", () => {
    const text = incompatibleRemedy(connection("remote"), { upgrade: true, connectAnyway: true, reconnect: true });
    expect(text).toMatch(/^Upgrade installs this app's version on that machine and restarts its daemon onto it; if agents are running there, you are asked before any is stopped\. Connect anyway uses /);
  });

  it("no longer withholds Replace daemon for running agents; it says it will ask", () => {
    const busy = { ...connection("local"), runningAgentCount: 3 };
    expect(incompatibleRemedy(busy, { replaceDaemon: true })).toBe("Replace daemon stops this daemon and starts the one that came with this app; 3 agents are running on it, and you are shown which before any is stopped.");
    expect(incompatibleRemedy({ ...busy, runningAgentCount: 0 }, { replaceDaemon: true })).toBe("Replace daemon stops this daemon and starts the one that came with this app.");
    expect(incompatibleRemedy(busy, { replaceDaemon: true })).not.toMatch(/not offered/);
  });
});
