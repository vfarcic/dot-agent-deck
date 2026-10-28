import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { createFixtureStartedAgent } from "../data/fixture";
import { OVERLAY_LAYER, topmostOverlayClose, useInertBackground, useOverlaySheet } from "../hooks/useInertBackground";
import { AgentTile } from "./AgentTile";
import { OutputReader } from "./OutputReader";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>synthetic terminal</pre> }));

const AGENT = createFixtureStartedAgent({ id: "7", daemonId: "deck-1709", displayName: "Coder", command: "claude", cwd: "/project" });

/** A dialog drawn over the reader, standing in for a stop confirmation. */
function Confirmation({ onClose }: { onClose: () => void }) {
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, onClose);
  return <section ref={ref} tabIndex={-1} role="alertdialog" aria-label="Close coder?">Close coder?</section>;
}

/** A sheet, standing in for Settings, which voice can open while the reader is up. */
function Sheet({ onClose }: { onClose: () => void }) {
  const { ref, raised } = useOverlaySheet<HTMLElement>(true, onClose);
  return <section ref={ref} tabIndex={-1} role="dialog" aria-label="Settings" data-raised={raised}>Settings</section>;
}

/* Issue #1709 — the output reader is on the overlay stack, so voice's "close"
   (which closes `topmostOverlayClose()`) dismisses it, and what is drawn over
   it closes first. */
describe("OutputReader on the overlay stack", () => {
  /** Scenario: Open an agent's Reader from its tile; the rest of the window is inert, voice's "close" closes the reader, and focus goes back to the Reader button with the window live again. */
  it("fences the window and closes on voice's close", () => {
    render(<><button data-testid="rail">Daemons</button><AgentTile agent={AGENT} presentation="tile" showDetails={false} mode="live"
      selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()} onTabChange={vi.fn()}
      onTerminalInput={vi.fn(async () => {})} onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()} /></>);
    const launcher = screen.getByTestId("reader-open-7");
    launcher.focus();
    fireEvent.click(launcher);
    const reader = screen.getByRole("dialog", { name: "Claude output reader" });
    expect(screen.getByTestId("rail")).toHaveAttribute("inert");
    expect(reader).not.toHaveAttribute("inert");
    expect(reader).toContainElement(document.activeElement as HTMLElement);

    act(() => { topmostOverlayClose()?.(); });
    expect(screen.queryByRole("dialog", { name: "Claude output reader" })).toBeNull();
    expect(screen.getByTestId("rail")).not.toHaveAttribute("inert");
    expect(document.activeElement).toBe(launcher);
    expect(topmostOverlayClose()).toBeUndefined();
  });

  /** Scenario: A confirmation opened over the reader is what "close" closes; the reader under it stays open, and the next "close" closes the reader. */
  it("is closed after a dialog drawn over it", () => {
    const closeReader = vi.fn();
    const closeConfirm = vi.fn();
    const { rerender } = render(<><OutputReader agent={AGENT} onClose={closeReader} /><Confirmation onClose={closeConfirm} /></>);
    expect(screen.getByTestId("reader-7")).toHaveAttribute("inert");

    act(() => { topmostOverlayClose()?.(); });
    expect(closeConfirm).toHaveBeenCalledTimes(1);
    expect(closeReader).not.toHaveBeenCalled();

    rerender(<><OutputReader agent={AGENT} onClose={closeReader} /></>);
    act(() => { topmostOverlayClose()?.(); });
    expect(closeReader).toHaveBeenCalledTimes(1);
  });

  /** Scenario: Settings opened while the reader is up is raised above it and live, not left inert under the reader's fence, and "close" closes Settings first. */
  it("raises a sheet opened over it", () => {
    const closeReader = vi.fn();
    const closeSheet = vi.fn();
    const { rerender } = render(<OutputReader agent={AGENT} onClose={closeReader} />);
    rerender(<><OutputReader agent={AGENT} onClose={closeReader} /><Sheet onClose={closeSheet} /></>);
    const sheet = screen.getByRole("dialog", { name: "Settings" });
    expect(sheet).toHaveAttribute("data-raised", "true");
    expect(sheet).not.toHaveAttribute("inert");
    expect(screen.getByTestId("reader-7")).toHaveAttribute("inert");

    act(() => { topmostOverlayClose()?.(); });
    expect(closeSheet).toHaveBeenCalledTimes(1);
    expect(closeReader).not.toHaveBeenCalled();
  });
});
