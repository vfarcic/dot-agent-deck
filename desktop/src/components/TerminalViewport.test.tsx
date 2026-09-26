import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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
const { terminals, FakeTerminal, FakeFitAddon } = vi.hoisted(() => {
  const terminals: FakeTerminal[] = [];
  /** Only the surface `TerminalViewport` actually touches. */
  class FakeTerminal {
    options: Record<string, unknown>;
    textarea: HTMLTextAreaElement;
    cols = 80;
    rows = 24;
    disposed = false;
    host?: HTMLElement;
    private readonly handlers = new Set<(data: string) => void>();
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      this.textarea = document.createElement("textarea");
      terminals.push(this);
    }
    loadAddon(addon: { activate?: (terminal: FakeTerminal) => void }): void { addon.activate?.(this); }
    open(host: HTMLElement): void { this.host = host; host.appendChild(this.textarea); }
    write(): void {}
    reset(): void {}
    focus(): void {}
    resize(cols: number, rows: number): void { this.cols = cols; this.rows = rows; }
    onData(handler: (data: string) => void): { dispose: () => void } {
      this.handlers.add(handler);
      return { dispose: () => { this.handlers.delete(handler); } };
    }
    /** A keystroke: what xterm hands the component's `onData` callback. */
    typeKey(data: string): void { for (const handler of [...this.handlers]) handler(data); }
    dispose(): void { this.disposed = true; }
  }
  /**
   * A deterministic layout stand-in. jsdom has no box model, so the fit addon
   * reads the component's presentation seam and supplies the two geometries a
   * browser layout produces for this test.
   */
  class FakeFitAddon {
    private terminal?: FakeTerminal;
    activate(terminal: FakeTerminal): void { this.terminal = terminal; }
    fit(): void {
      if (!this.terminal) return;
      const overlay = this.terminal.host?.closest('[data-presentation="overlay"]');
      this.terminal.cols = overlay ? 160 : 80;
      this.terminal.rows = overlay ? 48 : 24;
    }
    dispose(): void {}
  }
  return { terminals, FakeTerminal, FakeFitAddon };
});

vi.mock("@xterm/xterm", () => ({ Terminal: FakeTerminal as unknown as typeof import("@xterm/xterm").Terminal }));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: FakeFitAddon as unknown as typeof import("@xterm/addon-fit").FitAddon,
}));
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class {
    onContextLoss(): void {}
    dispose(): void {}
  } as unknown as typeof import("@xterm/addon-webgl").WebglAddon,
}));

import { DeckShell as AppDeckShell } from "../App";

/** The terminal case opens the deck explicitly. */
function DeckShell(props: Parameters<typeof AppDeckShell>[0]) {
  return <AppDeckShell initialView={{ kind: "deck" }} {...props} />;
}
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "../data/fixture";
import { agentKey } from "../lib/agentKey";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "../lib/bridge";
import type { AgentTarget, DeckActionResult, DeckRuntimeState } from "../types";
import { TerminalViewport } from "./TerminalViewport";

const resizeObservers: ControlledResizeObserver[] = [];

class ControlledResizeObserver implements ResizeObserver {
  target?: Element;
  constructor(private readonly callback: ResizeObserverCallback) { resizeObservers.push(this); }
  observe(target: Element): void { this.target = target; }
  unobserve(): void {}
  disconnect(): void {}
  trigger(): void { this.callback([], this); }
}

function triggerResize(agentId: string): void {
  const observer = resizeObservers.find((candidate) => candidate.target?.closest(`[data-testid="terminal-${agentId}"]`));
  if (!observer) throw new Error(`no resize observer for ${agentId}`);
  observer.trigger();
}

/**
 * The fake xterm behind a given agent's viewport — the handle the applied-grid
 * assertion needs, since `resizeTerminal` only ever sees what `fit()` PROPOSED
 * and never the grid xterm is left parsing at.
 */
function terminalFor(agentId: string) {
  const terminal = terminals.find((candidate) => candidate.host?.closest(`[data-testid="terminal-${agentId}"]`));
  if (!terminal) throw new Error(`no terminal for ${agentId}`);
  return terminal;
}

/**
 * The geometry the daemon has APPLIED for Planner. The fixture below hands it
 * to the runtime and the test asserts xterm is held at it, so the two can never
 * drift into agreeing by accident.
 */
const APPLIED_GRID = { cols: 80, rows: 24 };

function overlayRuntime(resizeTerminal: DeckRuntimeState["resizeTerminal"]): DeckRuntimeState {
  const snapshot = createFixtureSnapshot("connected");
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    // Keyed by the composite `(deckId, agentId)` the runtime uses since PRD
    // #1105's security audit. Only the FIXTURE moved: a bare id here would
    // simply never be found, and this test would then assert nothing about the
    // applied geometry it exists to hold xterm at.
    appliedGeometry: { [agentKey(snapshot.connection.deckId, "planner")]: APPLIED_GRID },
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal,
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  } as unknown as DeckRuntimeState;
}

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
   * the rebuild this avoids. The fake terminal above fires its handlers
   * directly, which is what lets each seam be asserted on its own; against the
   * real xterm they are layered rather than independent, and the seam-2 note
   * below says what that means for what this proves.
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
    // Seam 2: the guard reads the CURRENT value of `readOnly`, not the one
    // captured when the terminal was built — the assertion a stale closure
    // fails. What it proves is that the guard tracks current state, NOT that
    // the guard is the only thing standing between a read-only pane and the
    // agent: against the installed `@xterm/xterm@6.0.0`, `triggerDataEvent`
    // re-reads `rawOptions.disableStdin` on every keystroke and returns before
    // firing `onData` (`CoreService.ts:61-64`), so seam 1 suppresses a real
    // keystroke first and this guard is defense in depth behind it. A stale
    // closure here leaves one seam out of step with the other two, which is
    // what the fake terminal's direct handler call makes visible.
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

describe("TerminalViewport agent pane geometry", () => {
  const originalResizeObserver = globalThis.ResizeObserver;

  beforeEach(() => {
    terminals.length = 0;
    resizeObservers.length = 0;
    Object.defineProperty(window, "ResizeObserver", { value: ControlledResizeObserver, writable: true });
    Object.defineProperty(globalThis, "ResizeObserver", { value: ControlledResizeObserver, writable: true });
    window.localStorage.clear();
  });

  afterEach(() => {
    Object.defineProperty(window, "ResizeObserver", { value: originalResizeObserver, writable: true });
    Object.defineProperty(globalThis, "ResizeObserver", { value: originalResizeObserver, writable: true });
  });

  /**
   * Scenario: measure Planner as a deck tile, open its promoted full-window
   * pane, then close it. The resize callback reports a larger proposed grid
   * while open and the original tile grid after close — and xterm itself is put
   * straight back to the daemon-applied grid after the pane's larger proposal,
   * because the PTY is still at that size and parsing its bytes at the pane's
   * geometry is PRD #104's mis-parse relocated into the desktop.
   *
   * The two halves need separate observations and only one of them is
   * `resizeTerminal`: `fit()` reports the proposal BEFORE `applyAppliedGrid()`
   * runs, so the reported numbers are identical whether the applied geometry is
   * found, missing or ignored. The restoration is only visible on the terminal
   * instance, which is why `terminalFor` exists.
  */
  it("reports the pane's larger proposed grid and restores the applied grid", () => {
    const resizeTerminal = vi.fn(async (_target: AgentTarget, _cols: number, _rows: number) => undefined);
    render(<DeckShell runtime={overlayRuntime(resizeTerminal)} />);
    const terminal = terminalFor("planner");

    triggerResize("planner");
    expect(resizeTerminal).toHaveBeenLastCalledWith({ deckId: FIXTURE_DAEMON_ID, agentId: "planner" }, 80, 24);
    const [, tileCols, tileRows] = resizeTerminal.mock.calls.at(-1)!;
    resizeTerminal.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    triggerResize("planner");
    expect(resizeTerminal).toHaveBeenCalledTimes(1);
    const [, overlayCols, overlayRows] = resizeTerminal.mock.calls[0];
    expect(overlayCols).toBeGreaterThan(tileCols);
    expect(overlayRows).toBeGreaterThan(tileRows);
    // The pane PROPOSED 160x48 and the daemon has not applied it, so the grid
    // xterm is left at is the applied one — not the one just reported above.
    expect({ cols: terminal.cols, rows: terminal.rows }).toEqual(APPLIED_GRID);
    resizeTerminal.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
    triggerResize("planner");
    expect(resizeTerminal).toHaveBeenCalledTimes(1);
    expect(resizeTerminal).toHaveBeenLastCalledWith({ deckId: FIXTURE_DAEMON_ID, agentId: "planner" }, tileCols, tileRows);
  });
});
