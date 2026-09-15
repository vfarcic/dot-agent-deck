import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

/*
 * A fake xterm rather than the real one, for the reason this file exists: what
 * is under test is whether the terminal INSTANCE survives a lease flip, and an
 * assertion about that needs a handle on the instance. jsdom has no WebGL and
 * no layout engine either, so the real Terminal here would measure nothing and
 * still tell us nothing about identity.
 *
 * Hoisted so the `vi.mock` factories below — which vitest lifts above the
 * imports — can close over it.
 */
const { terminals, FakeTerminal } = vi.hoisted(() => {
  const terminals: FakeTerminal[] = [];
  /** Only the surface `TerminalViewport` actually touches. */
  class FakeTerminal {
    options: Record<string, unknown>;
    textarea: HTMLTextAreaElement;
    cols = 80;
    rows = 24;
    disposed = false;
    private readonly handlers = new Set<(data: string) => void>();
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      this.textarea = document.createElement("textarea");
      terminals.push(this);
    }
    loadAddon(): void {}
    open(host: HTMLElement): void { host.appendChild(this.textarea); }
    write(): void {}
    reset(): void {}
    focus(): void {}
    resize(): void {}
    onData(handler: (data: string) => void): { dispose: () => void } {
      this.handlers.add(handler);
      return { dispose: () => { this.handlers.delete(handler); } };
    }
    /** A keystroke: what xterm hands the component's `onData` callback. */
    typeKey(data: string): void { for (const handler of [...this.handlers]) handler(data); }
    dispose(): void { this.disposed = true; }
  }
  return { terminals, FakeTerminal };
});

vi.mock("@xterm/xterm", () => ({ Terminal: FakeTerminal as unknown as typeof import("@xterm/xterm").Terminal }));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class { fit(): void {} } as unknown as typeof import("@xterm/addon-fit").FitAddon,
}));
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class {
    onContextLoss(): void {}
    dispose(): void {}
  } as unknown as typeof import("@xterm/addon-webgl").WebglAddon,
}));

import { TerminalViewport } from "./TerminalViewport";

function renderViewport(readOnly: boolean, onInput: (data: string) => void, agentId = "planner") {
  return render(
    <TerminalViewport
      agentId={agentId}
      label="Planner"
      transcript=""
      readOnly={readOnly}
      onInput={onInput}
      onResize={() => {}}
    />,
  );
}

describe("TerminalViewport input gate", () => {
  beforeEach(() => { terminals.length = 0; });

  /**
   * Scenario: the operator is reading scrollback in a live terminal when a
   * second client attaches and the daemon hands it the write lease. The pane
   * goes read-only, and it must do so IN PLACE: rebuilding the terminal would
   * throw away scroll position and any in-progress selection (PRD #882 hands
   * the lease back and forth, so it would happen twice per handoff).
   *
   * All three seams have to move together — `disableStdin`, the `onData` guard
   * and the helper textarea's native `disabled` — because a terminal that
   * announces itself disabled while still accepting keystrokes is worse than
   * the rebuild this avoids.
   */
  it("reconciles all three input seams in place when the write lease is taken away", () => {
    const onInput = vi.fn();
    const { rerender } = renderViewport(false, onInput);

    expect(terminals).toHaveLength(1);
    const terminal = terminals[0];
    expect(terminal.options.disableStdin).toBe(false);
    expect(terminal.textarea.disabled).toBe(false);
    terminal.typeKey("a");
    expect(onInput).toHaveBeenCalledWith("a");
    expect(screen.getByTestId("terminal-planner")).toHaveAttribute("aria-disabled", "false");

    // Another client takes the write lease.
    rerender(
      <TerminalViewport agentId="planner" label="Planner" transcript="" readOnly onInput={onInput} onResize={() => {}} />,
    );

    // No rebuild: the same instance, never disposed.
    expect(terminals).toHaveLength(1);
    expect(terminals[0]).toBe(terminal);
    expect(terminal.disposed).toBe(false);
    // Seam 1: xterm ignores input.
    expect(terminal.options.disableStdin).toBe(true);
    // Seam 3: the helper textarea takes no focus and shows no caret.
    expect(terminal.textarea.disabled).toBe(true);
    // Seam 2: the guard reads the CURRENT value, not the one captured when the
    // terminal was built — this is the assertion a stale closure fails.
    onInput.mockClear();
    terminal.typeKey("b");
    expect(onInput).not.toHaveBeenCalled();
    expect(screen.getByTestId("terminal-planner")).toHaveAttribute("aria-disabled", "true");
  });

  /**
   * Scenario: the write lease comes back to this client. The same three seams
   * open again on the same terminal instance — the lease is bidirectional, so
   * a gate that only ever closes would leave the operator unable to type into
   * a pane the daemon has just handed back.
   */
  it("re-opens all three input seams when the write lease returns", () => {
    const onInput = vi.fn();
    const { rerender } = renderViewport(true, onInput);
    const terminal = terminals[0];
    expect(terminal.options.disableStdin).toBe(true);

    rerender(
      <TerminalViewport agentId="planner" label="Planner" transcript="" readOnly={false} onInput={onInput} onResize={() => {}} />,
    );

    expect(terminals).toHaveLength(1);
    expect(terminal.disposed).toBe(false);
    expect(terminal.options.disableStdin).toBe(false);
    expect(terminal.textarea.disabled).toBe(false);
    terminal.typeKey("c");
    expect(onInput).toHaveBeenCalledWith("c");
    expect(screen.getByTestId("terminal-planner")).toHaveAttribute("aria-disabled", "false");
  });

  /**
   * Scenario: a pane that is already unwritable when its tile mounts. The three
   * seams are set at construction rather than by the reconciling effect, so
   * this pins the other half of the same contract.
   */
  it("mounts a read-only pane with every seam already closed", () => {
    const onInput = vi.fn();
    renderViewport(true, onInput);

    const terminal = terminals[0];
    expect(terminal.options.disableStdin).toBe(true);
    expect(terminal.options.cursorBlink).toBe(false);
    expect(terminal.textarea.disabled).toBe(true);
    terminal.typeKey("d");
    expect(onInput).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the tile is pointed at a different agent. THAT is a rebuild — the
   * terminal owns one agent's buffer and cannot be re-pointed — so this pins
   * the one dependency the build effect still has, and proves the lease fix did
   * not collapse it too.
   */
  it("still rebuilds the terminal when the agent changes", () => {
    const onInput = vi.fn();
    const { rerender } = renderViewport(false, onInput);
    const first = terminals[0];

    rerender(
      <TerminalViewport agentId="coder" label="Coder" transcript="" readOnly={false} onInput={onInput} onResize={() => {}} />,
    );

    expect(terminals).toHaveLength(2);
    expect(first.disposed).toBe(true);
  });
});
