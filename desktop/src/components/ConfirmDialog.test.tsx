import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground, VOICE_PEER_PROPS } from "../hooks/useInertBackground";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";

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
