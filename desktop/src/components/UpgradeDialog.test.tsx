import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { UpgradeEvent, UpgradeOutcome, UpgradeStopSet } from "../lib/upgrade";
import { UpgradeDialog, type UpgradeTarget } from "./UpgradeDialog";

const TARGET: UpgradeTarget = { deckId: "deck-remote", deckName: "build-box", kind: "upgrade", offer: { kind: "offered", from: "0.44.0", to: "0.45.0" } };
const AT_STAKE: UpgradeStopSet = {
  agents: [{ id: "7", label: "coder", paneId: "2", cwd: "/work/app" }],
  roles: [{ paneId: "1", role: "orchestrator", orchestration: "tdd", isOrchestrator: true }],
};

/**
 * A runtime whose upgrade is driven step by step from the test: `emit` sends
 * an event to the dialog, `finish` resolves the run.
 */
function controlledRuntime() {
  let onEvent!: (event: UpgradeEvent) => void;
  let finish!: (outcome: UpgradeOutcome) => void;
  let fail!: (cause: Error) => void;
  const upgradeDaemon = vi.fn((_deckId: string, listener: (event: UpgradeEvent) => void) => {
    onEvent = listener;
    return new Promise<UpgradeOutcome>((resolve, reject) => { finish = resolve; fail = reject; });
  });
  const decideUpgrade = vi.fn(async () => undefined);
  return {
    runtime: { upgradeDaemon, decideUpgrade },
    emit: (event: UpgradeEvent) => act(() => onEvent(event)),
    finish: async (outcome: UpgradeOutcome) => { await act(async () => finish(outcome)); },
    fail: async (cause: Error) => { await act(async () => fail(cause)); },
  };
}

const progress = (stage: "installing" | "restarting" | "verifying"): UpgradeEvent => ({ type: "progress", deckId: "deck-remote", upgradeId: "upgrade-1", progress: { stage } });
const decision: UpgradeEvent = { type: "decision", deckId: "deck-remote", upgradeId: "upgrade-1", atStake: AT_STAKE, stale: false };

describe("UpgradeDialog", () => {
  /** Scenario: Confirms, shows each stage as it runs, then the outcome in plain words; Close dismisses it. */
  it("walks confirm → stages → outcome for an idle daemon", async () => {
    const { runtime, emit, finish } = controlledRuntime();
    const onClose = vi.fn();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={onClose} />);

    expect(screen.getByRole("alertdialog")).toHaveTextContent("Upgrade the daemon on build-box?");
    expect(screen.getByTestId("upgrade-confirm-body")).toHaveTextContent("This installs 0.45.0 on build-box (its daemon runs 0.44.0 now) and restarts the daemon onto it.");
    expect(runtime.upgradeDaemon).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("upgrade-start"));
    expect(runtime.upgradeDaemon).toHaveBeenCalledWith("deck-remote", expect.any(Function));

    emit(progress("installing"));
    expect(screen.getByTestId("upgrade-stage-installing")).toHaveAttribute("data-state", "active");
    expect(screen.getByTestId("upgrade-stage-restarting")).toHaveAttribute("data-state", "pending");
    emit(progress("restarting"));
    emit(progress("verifying"));
    expect(screen.getByTestId("upgrade-stage-installing")).toHaveAttribute("data-state", "done");
    expect(screen.getByTestId("upgrade-stage-verifying")).toHaveAttribute("data-state", "active");
    // Still working: Escape does not hide it.
    fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();

    await finish({ outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: { agents: [], roles: [] } });
    const outcome = screen.getByTestId("upgrade-outcome");
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Daemon upgraded");
    expect(outcome).toHaveTextContent("The daemon on build-box now runs 0.45.0 (it was 0.44.0).");
    expect(outcome).toHaveAttribute("data-tone", "success");
    fireEvent.click(screen.getByTestId("upgrade-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(runtime.decideUpgrade).not.toHaveBeenCalled();
  });

  /** Scenario: With agents running, names every agent and role, and Restart now sends the restart answer. */
  it("asks before stopping agents and sends Restart now", async () => {
    const { runtime, emit, finish } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(progress("installing"));
    emit(progress("restarting"));
    emit(decision);

    const question = screen.getByTestId("upgrade-decision");
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Restart and stop these?");
    expect(question).toHaveTextContent("Restarting the daemon on build-box stops 1 agent and 1 orchestration role:");
    const list = within(question).getByTestId("upgrade-at-stake");
    expect(list).toHaveTextContent("Agent coder (pane 2, in /work/app)");
    expect(list).toHaveTextContent("Role orchestrator of tdd, pane 1 (the orchestrator)");
    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-restart-now")); });
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", "restart-now");
    // Delivered: back to the stages while the restart runs.
    expect(screen.queryByTestId("upgrade-decision")).not.toBeInTheDocument();
    expect(screen.getByTestId("upgrade-stage-restarting")).toHaveAttribute("data-state", "active");

    emit(progress("verifying"));
    await finish({ outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: AT_STAKE });
    expect(screen.getByTestId("upgrade-outcome")).toHaveTextContent("These were stopped by the restart:");
    expect(screen.getByTestId("upgrade-outcome-list")).toHaveTextContent("Agent coder");
  });

  /** Scenario: Keep current daemon sends the keep answer and the outcome says the daemon kept running. */
  it("keeps the current daemon when told to", async () => {
    const { runtime, emit, finish } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(decision);
    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-keep-current")); });
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", "keep-current");
    await finish({ outcome: "installed-not-restarted", fromVersion: "0.44.0", installedVersion: "0.45.0", reason: { kind: "kept-by-user", atStake: AT_STAKE } });
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Daemon kept running");
    expect(screen.getByTestId("upgrade-outcome")).toHaveTextContent("0.45.0 is installed on build-box. The daemon keeps running 0.44.0, as you chose");
  });

  /** Scenario: Restart now is pressed while the answer is still on its way; the question stays on screen, a second press sends nothing more, and the stages come back once it has arrived. */
  it("keeps the question until the answer has been delivered", async () => {
    const { runtime, emit } = controlledRuntime();
    let deliver!: () => void;
    runtime.decideUpgrade.mockImplementationOnce(() => new Promise<undefined>((resolve) => { deliver = () => resolve(undefined); }));
    const onClose = vi.fn();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(progress("restarting"));
    emit(decision);

    fireEvent.click(screen.getByTestId("upgrade-restart-now"));
    expect(screen.getByTestId("upgrade-decision")).toHaveAttribute("aria-busy", "true");
    fireEvent.click(screen.getByTestId("upgrade-keep-current"));
    // Escape while the answer is on its way does not send a second one.
    fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" });
    expect(runtime.decideUpgrade).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();

    await act(async () => deliver());
    expect(screen.queryByTestId("upgrade-decision")).not.toBeInTheDocument();
    expect(screen.getByTestId("upgrade-stage-restarting")).toHaveAttribute("data-state", "active");
  });

  /** Scenario: Restart now does not reach Agent Deck; the question stays with a plain message saying so, and pressing Restart now again delivers it. */
  it("says so when an answer does not arrive, and lets it be sent again", async () => {
    const { runtime, emit } = controlledRuntime();
    runtime.decideUpgrade.mockImplementationOnce(() => Promise.reject(new Error("ipc: channel closed")));
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(decision);

    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-restart-now")); });
    expect(screen.getByTestId("upgrade-decision")).toBeInTheDocument();
    const error = screen.getByTestId("upgrade-decision-error");
    expect(error).toHaveTextContent("Your answer did not reach Agent Deck, so nothing has been stopped or restarted yet. Choose again to retry.");
    expect(error).not.toHaveTextContent("ipc");
    expect(screen.getByTestId("upgrade-at-stake")).toHaveTextContent("Agent coder");

    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-restart-now")); });
    expect(runtime.decideUpgrade).toHaveBeenCalledTimes(2);
    expect(runtime.decideUpgrade).toHaveBeenLastCalledWith("upgrade-1", "restart-now");
    expect(screen.queryByTestId("upgrade-decision")).not.toBeInTheDocument();
    expect(screen.queryByTestId("upgrade-decision-error")).not.toBeInTheDocument();
  });

  /** Scenario: An answer fails to arrive, and the user then presses Escape; that still answers Keep current daemon and closes the dialog. */
  it("still answers Keep current daemon when dismissed after a failed answer", async () => {
    const { runtime, emit } = controlledRuntime();
    runtime.decideUpgrade.mockImplementationOnce(() => Promise.reject(new Error("unreachable")));
    const onClose = vi.fn();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(decision);
    await act(async () => { fireEvent.click(screen.getByTestId("upgrade-restart-now")); });
    expect(screen.getByTestId("upgrade-decision-error")).toBeInTheDocument();

    fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" });
    expect(runtime.decideUpgrade).toHaveBeenLastCalledWith("upgrade-1", "keep-current");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /** Scenario: Escape or a click outside while the question is open answers Keep current daemon and closes the dialog. */
  it.each([
    ["Escape", () => fireEvent.keyDown(screen.getByRole("alertdialog"), { key: "Escape" })],
    ["a click outside", () => fireEvent.mouseDown(screen.getByRole("presentation"))],
  ])("treats %s during the question as Keep current daemon", (_how, dismiss) => {
    const { runtime, emit } = controlledRuntime();
    const onClose = vi.fn();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(decision);
    dismiss();
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", "keep-current");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /** Scenario: The dialog going away with the question open answers Keep current daemon. */
  it("answers Keep when it unmounts with the question open", () => {
    const { runtime, emit } = controlledRuntime();
    const view = render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(decision);
    view.unmount();
    expect(runtime.decideUpgrade).toHaveBeenCalledWith("upgrade-1", "keep-current");
  });

  /** Scenario: Says that the question changed when the daemon asks again with a different list. */
  it("says so when what is running changed since the last ask", () => {
    const { runtime, emit } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit({ ...decision, stale: true } as UpgradeEvent);
    expect(screen.getByTestId("upgrade-decision-stale")).toHaveTextContent("What is running changed since you were asked");
  });

  /** Scenario: A failure at a stage is shown in plain words with what is still running. */
  it("shows a failed stage", async () => {
    const { runtime, finish } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    await finish({ outcome: "failed", stage: "installing", reason: "ssh: connection timed out" });
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Upgrade failed");
    expect(screen.getByTestId("upgrade-outcome")).toHaveAttribute("data-tone", "failure");
    expect(screen.getByTestId("upgrade-outcome")).toHaveTextContent("It failed while installing the new version: ssh: connection timed out");
    expect(screen.getByTestId("upgrade-outcome")).toHaveTextContent("The daemon that was running keeps running.");
    expect(screen.getByTestId("upgrade-outcome")).not.toHaveTextContent("Nothing was changed");
  });

  /** Scenario: An upgrade that could not start (a second press, a deck not in the deck list) shows the reason. */
  it("shows why an upgrade could not start", async () => {
    const { runtime, fail } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    await fail(new Error("An upgrade of this daemon is already running in this app. Wait for it to finish."));
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Upgrade could not start");
    expect(screen.getByTestId("upgrade-error")).toHaveTextContent("already running in this app");
  });

  /** Scenario: Cancel on the confirmation closes the dialog and starts nothing. */
  it("starts nothing when cancelled", () => {
    const { runtime } = controlledRuntime();
    const onClose = vi.fn();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("upgrade-cancel"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(runtime.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: Replace daemon on the local deck is worded as a replacement, not an install. */
  it("words Replace daemon as a replacement", async () => {
    const { runtime, emit, finish } = controlledRuntime();
    render(<UpgradeDialog target={{ deckId: "deck-local", deckName: "/tmp/deck.sock", kind: "replace" }} runtime={runtime} onClose={vi.fn()} />);
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Replace the incompatible daemon?");
    expect(screen.getByTestId("upgrade-confirm-body")).toHaveTextContent("stops the daemon running on this machine and starts the one that came with this app");
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit(progress("installing"));
    expect(screen.getByTestId("upgrade-stage-installing")).toHaveTextContent("Preparing this app's daemon");
    await finish({ outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: { agents: [], roles: [] } });
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Daemon replaced");
    expect(screen.getByTestId("upgrade-outcome")).toHaveTextContent("The daemon on this machine now runs 0.45.0");
  });

  /**
   * Scenario: a remote daemon's stop set and outcome carry escape sequences,
   * line breaks and bidi overrides (PRD #1487 audit A3). The question and the
   * outcome render them as inert text: no control or bidi character reaches
   * the DOM, and every agent and role is still listed.
   */
  it("renders a hostile remote stop set and outcome as inert text", async () => {
    const hostile: UpgradeStopSet = {
      agents: [{ id: "7", label: "coder\u001b[2J\u001b]52;c;cm0=\u0007", paneId: "2\nKeep current daemon", cwd: "/work/\u202egnp.exe" }],
      roles: [{ paneId: "1\u0085", role: "lead\u009b31m", orchestration: "tdd\u2066", isOrchestrator: true }],
    };
    const unsafe = /[\u0000-\u001F\u007F-\u009F\u061c\u200e\u200f\u2028\u2029\u202a-\u202e\u2066-\u2069]/;
    const { runtime, emit, finish } = controlledRuntime();
    render(<UpgradeDialog target={TARGET} runtime={runtime} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("upgrade-start"));
    emit({ ...decision, atStake: hostile });
    const list = within(screen.getByTestId("upgrade-decision")).getByTestId("upgrade-at-stake");
    expect(list.textContent ?? "").not.toMatch(unsafe);
    expect(list).toHaveTextContent("Agent coder[2J]52;c;cm0= (pane 2Keep current daemon, in /work/gnp.exe)");
    expect(list).toHaveTextContent("Role lead31m of tdd, pane 1 (the orchestrator)");
    fireEvent.click(screen.getByTestId("upgrade-restart-now"));
    await finish({ outcome: "failed", stage: "restarting", reason: "refused\u001b]0;pwned\u0007\u202e", installedVersion: "0.45.0\u001b[31m" });
    const outcome = screen.getByTestId("upgrade-outcome");
    expect(outcome.textContent ?? "").not.toMatch(unsafe);
    expect(outcome).toHaveTextContent("refused]0;pwned");
  });
});
