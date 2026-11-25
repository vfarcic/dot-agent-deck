import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { UpgradeStopSet } from "../lib/upgrade";
import { UPGRADE_ALL_RUNNING_CLOSE_BLOCKED, upgradeAllOffered, type UpgradeAllEvent, type UpgradeAllPlan, type UpgradeAllReport } from "../lib/upgradeAll";
import { topmostOverlayClose } from "../hooks/useInertBackground";
import type { ConnectionView } from "../types";
import { UpgradeAllDialog } from "./UpgradeAllDialog";

const PLAN: UpgradeAllPlan = {
  batchId: "batch-1",
  headline: "Checked 3 remote decks: 2 to upgrade.",
  toUpgrade: 2,
  decks: [
    { deck: "mac-studio", deckId: "deck-a", kind: "upgrade", line: "Will be upgraded from 0.51.0 to 0.52.0." },
    { deck: "pi", deckId: "deck-b", kind: "skip", line: "Skipped: already runs 0.52.0." },
    { deck: "build-box", deckId: "deck-c", kind: "upgrade", line: "Will be upgraded from 0.51.0 to 0.52.0." },
  ],
};

const AT_STAKE: UpgradeStopSet = {
  agents: [{ id: "7", label: "coder", paneId: "2", cwd: "/work/app" }],
  roles: [{ paneId: "1", role: "orchestrator", orchestration: "tdd", isOrchestrator: true }],
};

/** A runtime whose check and run the test drives step by step. */
function controlledRuntime(plan: UpgradeAllPlan = PLAN) {
  let onEvent!: (event: UpgradeAllEvent) => void;
  let finish!: (report: UpgradeAllReport) => void;
  let checked!: (plan: UpgradeAllPlan) => void;
  const check = vi.fn(() => new Promise<UpgradeAllPlan>((resolve) => { checked = resolve; }));
  const run = vi.fn((_batchId: string, listener: (event: UpgradeAllEvent) => void) => {
    onEvent = listener;
    return new Promise<UpgradeAllReport>((resolve) => { finish = resolve; });
  });
  const stop = vi.fn(async () => undefined);
  const decideUpgrade = vi.fn(async () => undefined);
  return {
    runtime: { upgradeAll: { check, run, stop }, decideUpgrade },
    check: async () => { await act(async () => checked(plan)); },
    emit: (event: UpgradeAllEvent) => act(() => onEvent(event)),
    finish: async (report: UpgradeAllReport) => { await act(async () => finish(report)); },
  };
}

const deckLines = () => screen.getAllByTestId("upgrade-all-deck").map((row) => row.textContent);

describe("UpgradeAllDialog", () => {
  /** Scenario: Open Upgrade all: it checks every deck, shows each deck's plan in the crate's words, and Cancel closes without running anything. */
  it("shows the plan for every deck and Cancel runs nothing", async () => {
    const { runtime, check } = controlledRuntime();
    const onClose = vi.fn();
    render(<UpgradeAllDialog runtime={runtime} onClose={onClose} />);
    expect(screen.getByTestId("upgrade-all-checking")).toHaveTextContent("Checking every remote deck on the deck list…");
    await check();
    expect(screen.getByTestId("upgrade-all-headline")).toHaveTextContent("Checked 3 remote decks: 2 to upgrade.");
    expect(deckLines()).toEqual([
      "mac-studio: Will be upgraded from 0.51.0 to 0.52.0.",
      "pi: Skipped: already runs 0.52.0.",
      "build-box: Will be upgraded from 0.51.0 to 0.52.0.",
    ]);
    expect(screen.getByTestId("upgrade-all-question")).toHaveTextContent("Upgrade 2 daemons? Each daemon with agents or orchestration roles running asks before it stops any.");
    expect(screen.getByTestId("upgrade-all-cancel")).toHaveFocus();
    fireEvent.click(screen.getByTestId("upgrade-all-cancel"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(runtime.upgradeAll.run).not.toHaveBeenCalled();
  });

  /** Scenario: Upgrade runs the checked batch, shows each deck as it goes, asks a busy daemon about its own agents and roles, sends Restart now, and ends with one line per deck. */
  it("runs the batch, asks per daemon, and shows the results", async () => {
    const { runtime, check, emit, finish } = controlledRuntime();
    const onClose = vi.fn();
    render(<UpgradeAllDialog runtime={runtime} onClose={onClose} />);
    await check();
    fireEvent.click(screen.getByTestId("upgrade-all-start"));
    expect(runtime.upgradeAll.run).toHaveBeenCalledWith("batch-1", expect.any(Function));

    emit({ type: "started", index: 0 });
    emit({ type: "progress", index: 0, stage: "restarting" });
    expect(deckLines()).toEqual([
      "mac-studio: Upgrading: restarting the daemon…",
      "pi: Skipped: already runs 0.52.0.",
      "build-box: Waiting to be upgraded.",
    ]);
    emit({ type: "decision", deckId: "deck-a", attemptId: "attempt-1", upgradeId: "upgrade-1", questionId: 4, atStake: AT_STAKE, stale: false });
    const decision = screen.getByTestId("upgrade-all-decision");
    expect(decision).toHaveTextContent("Restarting the daemon on mac-studio stops 1 agent and 1 orchestration role:");
    expect(screen.getByTestId("upgrade-all-at-stake")).toHaveTextContent("Agent coder (pane 2, in /work/app)");
    expect(screen.getByTestId("upgrade-all-at-stake")).toHaveTextContent("Role orchestrator of tdd, pane 1 (the orchestrator)");
    expect(screen.getByTestId("upgrade-all-keep-current")).toHaveFocus();
    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-all-restart-now")); });
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", 4, "restart-now");
    expect(screen.queryByTestId("upgrade-all-decision")).toBeNull();

    emit({ type: "finished", index: 0, report: { deck: "mac-studio", deckId: "deck-a", status: "upgraded", line: "Upgraded: now runs 0.52.0 (was 0.51.0). Stopped 1 agent and 1 orchestration role.", endsSessions: true } });
    await finish({
      headline: "Upgrade all: 1 upgraded, 1 skipped, 1 failed of 3 remote decks.",
      failed: true,
      decks: [
        { deck: "mac-studio", deckId: "deck-a", status: "upgraded", line: "Upgraded: now runs 0.52.0 (was 0.51.0). Stopped 1 agent and 1 orchestration role.", endsSessions: true },
        { deck: "pi", deckId: "deck-b", status: "skipped", line: "Skipped: already runs 0.52.0.", endsSessions: false },
        { deck: "build-box", deckId: "deck-c", status: "failed", line: "Failed while installing the new version: the download failed", endsSessions: false },
      ],
    });
    expect(screen.getByTestId("upgrade-all-results")).toHaveAttribute("data-failed", "true");
    expect(screen.getByTestId("upgrade-all-headline")).toHaveTextContent("Upgrade all: 1 upgraded, 1 skipped, 1 failed of 3 remote decks.");
    expect(screen.getAllByTestId("upgrade-all-deck").map((row) => row.getAttribute("data-status"))).toEqual(["upgraded", "skipped", "failed"]);
    fireEvent.click(screen.getByTestId("upgrade-all-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /** Scenario: Escape at a daemon's question keeps its current daemon, and Escape while the batch runs is Stop after this daemon, which voice's close does not do. */
  it("keeps on Escape at a question and stops after this daemon on Escape while running", async () => {
    const { runtime, check, emit } = controlledRuntime();
    render(<UpgradeAllDialog runtime={runtime} onClose={vi.fn()} />);
    await check();
    fireEvent.click(screen.getByTestId("upgrade-all-start"));
    emit({ type: "started", index: 0 });
    emit({ type: "decision", deckId: "deck-a", attemptId: "attempt-1", upgradeId: "upgrade-1", questionId: 2, atStake: AT_STAKE, stale: false });
    await act(async () => { fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" }); });
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", 2, "keep-current");

    expect(topmostOverlayClose()?.()).toBe(UPGRADE_ALL_RUNNING_CLOSE_BLOCKED);
    expect(runtime.upgradeAll.stop).not.toHaveBeenCalled();
    await act(async () => { fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" }); });
    expect(runtime.upgradeAll.stop).toHaveBeenCalledWith("batch-1");
    expect(screen.getByTestId("upgrade-all-headline")).toHaveTextContent("Stopping after this daemon…");
    expect(screen.queryByTestId("upgrade-all-stop")).toBeNull();
  });

  /** Scenario: A dialog that goes away while a daemon asks answers Keep current daemon, so the batch never waits on a question nobody sees. */
  it("answers Keep when it goes away with a question open", async () => {
    const { runtime, check, emit } = controlledRuntime();
    const { unmount } = render(<UpgradeAllDialog runtime={runtime} onClose={vi.fn()} />);
    await check();
    fireEvent.click(screen.getByTestId("upgrade-all-start"));
    emit({ type: "decision", deckId: "deck-c", attemptId: "attempt-1", upgradeId: "upgrade-1", questionId: 9, atStake: AT_STAKE, stale: false });
    unmount();
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", 9, "keep-current");
  });

  /** Scenario: A plan with nothing older than this app says so and offers only Close. */
  it("offers only Close when nothing needs upgrading", async () => {
    const { runtime, check } = controlledRuntime({ batchId: "batch-2", headline: "Checked 1 remote deck: 0 to upgrade.", toUpgrade: 0, decks: [PLAN.decks[1]!] });
    render(<UpgradeAllDialog runtime={runtime} onClose={vi.fn()} />);
    await check();
    expect(screen.getByTestId("upgrade-all-question")).toHaveTextContent("Nothing to upgrade.");
    expect(screen.queryByTestId("upgrade-all-start")).toBeNull();
    expect(screen.getByTestId("upgrade-all-close")).toBeInTheDocument();
  });
});

describe("upgradeAllOffered", () => {
  const connection = (deckKind: "remote" | "local", kind: "offered" | "current"): ConnectionView => ({
    status: "connected",
    deckKind,
    upgradeOffer: kind === "offered" ? { kind, from: "0.51.0", to: "0.52.0" } : { kind },
  } as ConnectionView);

  /** Scenario: Upgrade all is offered while any remote deck runs an older daemon, and not for the local one alone. */
  it("is offered for an older remote deck only", () => {
    expect(upgradeAllOffered([connection("remote", "current"), connection("remote", "offered")])).toBe(true);
    expect(upgradeAllOffered([connection("local", "offered"), connection("remote", "current")])).toBe(false);
    expect(upgradeAllOffered([])).toBe(false);
  });
});
