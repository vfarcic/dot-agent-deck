import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground, VOICE_PEER_PROPS } from "../hooks/useInertBackground";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import type { WorktreeCloseReport } from "../lib/worktreeClose";

const STATE: ConfirmState = {
  title: "Close coder?",
  body: "This sends a stop request to coder. Unsaved terminal work may be interrupted.",
  label: "Close agent",
  busyLabel: "Stopping…",
  action: async () => undefined,
};

/** An overlay already on the stack, standing in for the pull request browser. */
function Page({ onClose }: { onClose: () => void }) {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.pullRequest, onClose);
  return <div ref={ref} data-testid="page">pull request</div>;
}

/* Issue #1709 — the confirmation is on the overlay stack, so voice's "close"
   (which closes `topmostOverlayClose()`) dismisses it. */
describe("ConfirmDialog on the overlay stack", () => {
  /** Scenario: while a worktree preview is pending, Cancel remains available but Close waits. Replacing that question ignores its late answer, and a failed preview lets the new close proceed without claiming that a worktree will be kept. */
  it("waits for the worktree hint without applying a cancelled hint to another close", async () => {
    let settle!: (report: WorktreeCloseReport) => void;
    const action = vi.fn(async () => {});
    const first = { ...STATE, action, worktreePreview: () => new Promise<WorktreeCloseReport>((resolve) => { settle = resolve; }) };
    const onClose = vi.fn();
    const { rerender } = render(<ConfirmDialog state={first} onClose={onClose} />);
    const pending = screen.getByRole("button", { name: "Checking worktree…" });
    expect(pending).toBeDisabled();
    fireEvent.click(pending);
    expect(action).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledOnce();
    const second = { ...STATE, title: "Close tester?", action, worktreePreview: async () => { throw new Error("daemon unavailable"); } };
    rerender(<ConfirmDialog state={second} onClose={onClose} />);
    await act(async () => settle({ kept: { path: "/old/dirty", confirmed_dirty: true }, keepWorktree: "/old/dirty" }));
    expect(screen.getByRole("alertdialog")).not.toHaveTextContent("/old/dirty");
    expect(screen.getByRole("button", { name: "Close agent" })).toBeEnabled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Close agent" })));
    expect(action).toHaveBeenCalledOnce();
  });

  /**
   * Scenario (issues #1958 and #2436): a close whose preview names a worktree
   * the deck can keep shows an unticked "Keep the worktree" checkbox under the
   * preview, beside two buttons; pressing Close with it ticked runs the close
   * with the keep, and closes the confirmation. Without a named worktree — or
   * for a confirmation that cannot keep one — there is no checkbox.
   */
  it("offers the keep-worktree checkbox only when the preview names a worktree to keep", async () => {
    const action = vi.fn(async () => {});
    const offered: ConfirmState = { ...STATE, action, keepWorktree: true, worktreePreview: async () => ({ kept: null, keepWorktree: "/w/unit" }) };
    const onClose = vi.fn();
    const first = render(<ConfirmDialog state={offered} onClose={onClose} />);
    expect(screen.queryByRole("checkbox")).toBeNull();
    const keep = await screen.findByRole("checkbox", { name: "Keep the worktree" });
    expect(keep).not.toBeChecked();
    expect(keep.closest("label")).toHaveAttribute("title", "Keeps /w/unit");
    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual(["Cancel", "Close agent"]);
    fireEvent.click(keep);
    expect(keep).toBeChecked();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Close agent" })));
    expect(action).toHaveBeenCalledExactlyOnceWith({ keepWorktree: true });
    expect(onClose).toHaveBeenCalledOnce();

    first.unmount();

    /* A newer confirmation starts unticked, and an unticked close does not keep. */
    const fresh = vi.fn(async () => {});
    const unticked: ConfirmState = { ...offered, title: "Close tester?", action: fresh };
    const { rerender } = render(<ConfirmDialog state={offered} onClose={onClose} />);
    fireEvent.click(await screen.findByRole("checkbox", { name: "Keep the worktree" }));
    rerender(<ConfirmDialog state={unticked} onClose={onClose} />);
    expect(await screen.findByRole("checkbox", { name: "Keep the worktree" })).not.toBeChecked();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Close agent" })));
    expect(fresh).toHaveBeenCalledExactlyOnceWith({ keepWorktree: false });
    rerender(<></>);

    for (const state of [
      { ...STATE, action, keepWorktree: true, worktreePreview: async () => ({ kept: null, keepWorktree: null }) },
      { ...STATE, action, keepWorktree: true, worktreePreview: async () => { throw new Error("daemon unavailable"); } },
      { ...STATE, action, worktreePreview: async () => ({ kept: null, keepWorktree: "/w/unit" }) },
      { ...STATE, action, keepWorktree: true },
    ] satisfies ConfirmState[]) {
      rerender(<ConfirmDialog state={state} onClose={onClose} />);
      await screen.findByRole("button", { name: "Close agent" });
      await act(async () => undefined);
      expect(screen.queryByRole("checkbox")).toBeNull();
      expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual(["Cancel", "Close agent"]);
    }
  });

  /**
   * Scenario (issue #2436): a confirmation with a second close shows three
   * buttons in one row — Cancel, the close and the second close — and fills
   * whichever the opener leads with. Pressing the second close runs only its
   * own action, with the keep-worktree box's answer.
   */
  it("offers a second close and leads with the one the opener chose", async () => {
    const action = vi.fn(async () => {});
    const whole = vi.fn(async () => {});
    const state: ConfirmState = { ...STATE, action, keepWorktree: true, worktreePreview: async () => ({ kept: null, keepWorktree: "/w/unit" }), alternative: { label: "Close orchestration", action: whole } };
    const onClose = vi.fn();
    const { rerender } = render(<ConfirmDialog state={state} onClose={onClose} />);
    await screen.findByRole("checkbox", { name: "Keep the worktree" });
    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual(["Cancel", "Close agent", "Close orchestration"]);
    expect(screen.getByRole("button", { name: "Close agent" })).toHaveClass("leads");
    expect(screen.getByRole("button", { name: "Close orchestration" })).not.toHaveClass("leads");

    const led: ConfirmState = { ...state, alternative: { label: "Close orchestration", action: whole, primary: true } };
    rerender(<ConfirmDialog state={led} onClose={onClose} />);
    expect(await screen.findByRole("button", { name: "Close orchestration" })).toHaveClass("leads");
    expect(screen.getByRole("button", { name: "Close agent" })).not.toHaveClass("leads");
    fireEvent.click(await screen.findByRole("checkbox", { name: "Keep the worktree" }));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Close orchestration" })));
    expect(whole).toHaveBeenCalledExactlyOnceWith({ keepWorktree: true });
    expect(action).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledOnce();
  });

  /** Scenario: With a confirmation open, the rest of the window is inert and voice's "close" is its Cancel; once it is gone the window is live again. */
  it("fences the window and closes on voice's close", () => {
    const onClose = vi.fn();
    const { rerender } = render(<><button data-testid="rail">Daemons</button><ConfirmDialog state={STATE} onClose={onClose} /></>);
    expect(screen.getByTestId("rail")).toHaveAttribute("inert");
    expect(screen.getByRole("alertdialog")).not.toHaveAttribute("inert");

    act(() => { topmostOverlayClose()?.(); });
    expect(onClose).toHaveBeenCalledTimes(1);

    rerender(<><button data-testid="rail">Daemons</button></>);
    expect(screen.getByTestId("rail")).not.toHaveAttribute("inert");
    expect(topmostOverlayClose()).toBeUndefined();
  });

  /** Scenario: Qodo's case on PR #1680 — a confirmation over the pull request page is what "close" closes; the page under it stays. */
  it("is closed before the page it is drawn over", () => {
    const closeConfirm = vi.fn();
    const closePage = vi.fn();
    render(<><Page onClose={closePage} /><ConfirmDialog state={STATE} onClose={closeConfirm} /></>);
    expect(screen.getByTestId("page")).toHaveAttribute("inert");

    act(() => { topmostOverlayClose()?.(); });
    expect(closeConfirm).toHaveBeenCalledTimes(1);
    expect(closePage).not.toHaveBeenCalled();
  });

  /**
   * Scenario: Qodo on PR #1804 — Confirm starts a slow action, "close" dismisses
   * the confirmation while it runs, and a second confirmation opens; when the
   * first action settles, the second confirmation is still open.
   */
  it("does not let a dismissed action close a newer confirmation", async () => {
    let settle!: () => void;
    const slow: ConfirmState = { ...STATE, action: () => new Promise<void>((resolve) => { settle = resolve; }) };
    const second: ConfirmState = { ...STATE, title: "Close tester?" };
    const onClose = vi.fn();
    const { rerender } = render(<ConfirmDialog state={slow} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Close agent" }));
    act(() => { topmostOverlayClose()?.(); });
    expect(onClose).toHaveBeenCalledTimes(1);

    rerender(<></>);
    rerender(<ConfirmDialog state={second} onClose={onClose} />);
    await act(async () => { settle(); });
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Close tester?");

    /* The same instance given a newer confirmation directly is not closed either. */
    let settleAgain!: () => void;
    rerender(<ConfirmDialog state={{ ...STATE, action: () => new Promise<void>((resolve) => { settleAgain = resolve; }) }} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Close agent" }));
    rerender(<ConfirmDialog state={second} onClose={onClose} />);
    await act(async () => { settleAgain(); });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /** Scenario: Confirm runs the action and, once it settles, closes the confirmation it ran for. */
  it("closes once its own action settles", async () => {
    const onClose = vi.fn();
    render(<ConfirmDialog state={STATE} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Close agent" }));
    await act(async () => undefined);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  /** Scenario: The voice surface's own "Always allow?" confirmation stays off the stack: it fences nothing and "close" does not reach it, as before. */
  it("stays off the stack inside the voice surface", () => {
    const onClose = vi.fn();
    render(<><button data-testid="rail">Daemons</button><div {...VOICE_PEER_PROPS}><ConfirmDialog state={STATE} onClose={onClose} /></div></>);
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    expect(screen.getByTestId("rail")).not.toHaveAttribute("inert");
    expect(topmostOverlayClose()).toBeUndefined();
  });
});
