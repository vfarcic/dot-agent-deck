import { act, render, screen } from "@testing-library/react";
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

  /** Scenario: The voice surface's own "Always allow?" confirmation stays off the stack: it fences nothing and "close" does not reach it, as before. */
  it("stays off the stack inside the voice surface", () => {
    const onClose = vi.fn();
    render(<><button data-testid="rail">Daemons</button><div {...VOICE_PEER_PROPS}><ConfirmDialog state={STATE} onClose={onClose} /></div></>);
    expect(screen.getByRole("alertdialog")).toBeInTheDocument();
    expect(screen.getByTestId("rail")).not.toHaveAttribute("inert");
    expect(topmostOverlayClose()).toBeUndefined();
  });
});
