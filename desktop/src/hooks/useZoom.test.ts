import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useZoom, ZOOM_PERSIST_INTERVAL_MS } from "./useZoom";
import { registerRefit, unregisterRefit } from "../lib/terminalRegistry";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "../lib/bridge";
import type { DesktopSettingsState } from "./useDesktopSettings";
import type { DeckRuntimeState } from "../types";

/**
 * The hook is driven in isolation rather than through a mounted `ControlDeck`,
 * and that is deliberate rather than convenient.
 *
 * `App.tsx`'s own `keydown` listener computes `editing` from
 * `event.target as HTMLElement` on every keydown, ahead of every branch, so a
 * keydown dispatched at `window` — which has no `.matches` — throws inside it.
 * That is issue #826, it is not this PRD's to fix, and mounting the deck here
 * would make these tests fail for a reason that has nothing to do with zoom.
 * The hook itself never reads `event.target`, which is why driving it directly
 * is honest and not a dodge.
 */
function settingsState(level = 1, overrides: Partial<DesktopSettingsState> = {}): DesktopSettingsState & { saved: DesktopSettingsDto[] } {
  const saved: DesktopSettingsDto[] = [];
  return {
    settings: { ...DEFAULT_DESKTOP_SETTINGS, zoom: { level } },
    loaded: true,
    save: vi.fn((next: DesktopSettingsDto) => { saved.push(next); }),
    saved,
    ...overrides,
  };
}

/**
 * Only the two fields `useZoom` reads, cast up to the full runtime — the hook
 * takes `DeckRuntimeState` because that is what `App.tsx` has, not because it
 * needs any of the rest, and building a whole runtime here would obscure that.
 */
function runtimeState(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "live",
    setZoom: vi.fn(async (level: number) => level),
    ...overrides,
  } as unknown as DeckRuntimeState;
}

function press(key: string, init: Partial<KeyboardEventInit> = {}, target: EventTarget = document.body): void {
  target.dispatchEvent(new KeyboardEvent("keydown", { key, ctrlKey: true, bubbles: true, cancelable: true, ...init }));
}

describe("useZoom", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
  });

  it("steps the level on the platform's zoom keys", () => {
    const settings = settingsState();
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settings));

    act(() => press("="));
    expect(runtime.setZoom).toHaveBeenCalledWith(1.1);

    act(() => press("-"));
    // Stepping is against the level the hook is holding, not the stale prop —
    // the settings state does not re-render in this harness, so a hook that
    // read only the prop would bounce between 1.1 and 0.9 forever.
    expect(runtime.setZoom).toHaveBeenLastCalledWith(1);
  });

  it("resets on the platform's reset key", () => {
    const settings = settingsState(2.5);
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settings));

    act(() => press("0"));
    expect(runtime.setZoom).toHaveBeenCalledWith(1);
  });

  it("saturates at both ends and stops calling the bridge there", () => {
    const settings = settingsState(3);
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settings));

    act(() => press("="));
    // No call at all, rather than a call with the same level: a keystroke that
    // cannot change anything must not cost an IPC, a re-fit or a disk write.
    expect(runtime.setZoom).not.toHaveBeenCalled();

    act(() => press("-"));
    expect(runtime.setZoom).toHaveBeenCalledWith(2.5);
  });

  it("ignores keys it does not own", () => {
    const settings = settingsState();
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settings));

    for (const key of ["k", "Escape", "?", "1", "j", "a"]) act(() => press(key));
    act(() => press("=", { ctrlKey: false }));
    expect(runtime.setZoom).not.toHaveBeenCalled();
  });

  /**
   * **The load-bearing test of this PRD.**
   *
   * xterm binds its own `keydown` on `.xterm-helper-textarea`, focused whenever
   * an agent pane has focus, and maps `Ctrl` plus a single-character key with
   * `keyCode >= 48` to that literal character (and `Ctrl _` to 0x1F). In the
   * bubble phase this hook would run *after* xterm had written a byte to the
   * focused agent's PTY, and `preventDefault` cannot retract a sent byte.
   *
   * The stand-in below is a listener on a descendant. If the hook's listener
   * loses its capture flag or its `stopPropagation`, that listener starts
   * firing and this test goes red — which is the only automated warning that
   * the zoom keys have started typing into somebody's agent.
   *
   * Worth knowing why this cannot be left to a manual check: the bug is
   * platform-split. The macOS binding uses `metaKey`, which xterm's branch
   * excludes, so a Mac reviewer would see nothing wrong.
   */
  it("claims the keys before a descendant listener can see them", () => {
    const settings = settingsState();
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settings));

    const pane = document.createElement("textarea");
    pane.className = "xterm-helper-textarea";
    document.body.appendChild(pane);
    const xtermWouldSee = vi.fn();
    pane.addEventListener("keydown", xtermWouldSee);

    try {
      for (const key of ["-", "=", "0", "_"]) {
        act(() => press(key, {}, pane));
      }
      expect(xtermWouldSee).not.toHaveBeenCalled();
      // And the zoom did happen, so this is not passing because the hook is
      // simply inert.
      expect(runtime.setZoom).toHaveBeenCalled();

      // A key the hook does not own still reaches the pane, so the interception
      // is scoped to the zoom keys rather than swallowing the keyboard.
      act(() => press("a", {}, pane));
      expect(xtermWouldSee).toHaveBeenCalledTimes(1);
    } finally {
      pane.remove();
    }
  });

  it("marks the event handled, so a browser default cannot also fire", () => {
    const settings = settingsState();
    renderHook(() => useZoom(runtimeState(), settings));

    const event = new KeyboardEvent("keydown", { key: "=", ctrlKey: true, bubbles: true, cancelable: true });
    act(() => { document.body.dispatchEvent(event); });
    expect(event.defaultPrevented).toBe(true);
  });

  it("re-fits every mounted pane on a zoom change", () => {
    const settings = settingsState();
    renderHook(() => useZoom(runtimeState(), settings));

    const refit = vi.fn();
    registerRefit("agent-1", refit);
    const second = vi.fn();
    registerRefit("agent-2", second);

    try {
      act(() => press("="));
      // The re-fit is frame-coalesced, so it has not run yet.
      expect(refit).not.toHaveBeenCalled();
      act(() => { vi.advanceTimersByTime(20); });
      expect(refit).toHaveBeenCalledTimes(1);
      expect(second).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("agent-1", refit);
      unregisterRefit("agent-2", second);
    }
  });

  it("collapses a burst of keystrokes into one re-fit pass per frame", () => {
    const settings = settingsState(0.75);
    renderHook(() => useZoom(runtimeState(), settings));

    const refit = vi.fn();
    registerRefit("agent-1", refit);
    try {
      // Five steps in one frame — what a held key produces. Every one of them
      // must reach the webview, but the layout must only be measured once.
      act(() => { for (let i = 0; i < 5; i += 1) press("="); });
      act(() => { vi.advanceTimersByTime(20); });
      expect(refit).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("agent-1", refit);
    }
  });

  it("coalesces the disk write, and the value that lands is the last one chosen", () => {
    const settings = settingsState(0.75);
    renderHook(() => useZoom(runtimeState(), settings));

    act(() => { for (let i = 0; i < 6; i += 1) press("="); });
    // Nothing on disk yet: the whole point is that a held key does not rewrite
    // desktop.toml once per key repeat.
    expect(settings.saved).toHaveLength(0);

    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved).toHaveLength(1);
    // 0.75 stepped six rungs is 1.75, and that is what was written — the
    // intermediate levels are dropped, not queued.
    expect(settings.saved[0].zoom.level).toBe(1.75);
  });

  it("saves the whole document, so it cannot drop a section it does not know about", () => {
    const settings = settingsState();
    settings.settings = { ...settings.settings, appearance: { mode: "dark" } };
    renderHook(() => useZoom(runtimeState(), settings));

    act(() => press("="));
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved[0]).toEqual({
      ...DEFAULT_DESKTOP_SETTINGS,
      appearance: { mode: "dark" },
      zoom: { level: 1.1 },
    });
  });

  /**
   * A trailing flush rather than a debounce that can starve.
   *
   * A pure debounce writes nothing while the key is still down, so a ten-second
   * hold followed by a crash loses the level entirely. This one writes once per
   * interval for as long as the burst lasts.
   */
  it("keeps writing during a long burst rather than starving until it ends", () => {
    const settings = settingsState(0.75);
    renderHook(() => useZoom(runtimeState(), settings));

    act(() => press("="));
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved).toHaveLength(1);

    act(() => press("="));
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved).toHaveLength(2);
    expect(settings.saved[1].zoom.level).toBe(1.0);
  });

  it("flushes a pending level on unmount, so the last change of a session survives", () => {
    const settings = settingsState();
    const { unmount } = renderHook(() => useZoom(runtimeState(), settings));

    act(() => press("="));
    expect(settings.saved).toHaveLength(0);
    unmount();
    expect(settings.saved).toHaveLength(1);
    expect(settings.saved[0].zoom.level).toBe(1.1);
  });

  /**
   * The launch-time apply belongs to `run()`'s `setup` hook in Rust, and this
   * is what keeps the frontend from fighting it.
   *
   * `useDesktopSettings` starts at the defaults and resolves asynchronously, so
   * the first level this hook ever sees is 100% whatever is on disk. An effect
   * that applied its initial value would slam a user stored at 150% back to
   * 100% and then jump them forward a moment later — which is exactly the flash
   * the Rust leg exists to prevent, reintroduced by the frontend.
   */
  it("does not apply the level it was seeded with", () => {
    const runtime = runtimeState();
    renderHook(() => useZoom(runtime, settingsState(1.5)));
    expect(runtime.setZoom).not.toHaveBeenCalled();
  });

  it("applies the stored level once the asynchronous load resolves", () => {
    const runtime = runtimeState();
    // Mount at the defaults, the way the real hook does before `getSettings`
    // has answered, then let the document arrive.
    const { rerender } = renderHook(({ settings }) => useZoom(runtime, settings), {
      initialProps: { settings: settingsState(1) },
    });
    expect(runtime.setZoom).not.toHaveBeenCalled();

    rerender({ settings: settingsState(1.5) });
    expect(runtime.setZoom).toHaveBeenCalledWith(1.5);
  });

  // The Settings row is an ordinary `SettingsPanelProps` panel, so its only
  // channel is the document. This is the path that makes it work without the
  // panel knowing this hook exists.
  it("adopts a stored level written by something other than the keys", () => {
    const runtime = runtimeState();
    const { result, rerender } = renderHook(({ settings }) => useZoom(runtime, settings), {
      initialProps: { settings: settingsState(1) },
    });

    act(() => press("="));
    expect(result.current.level).toBe(1.1);

    rerender({ settings: settingsState(2) });
    expect(result.current.level).toBe(2);
    expect(runtime.setZoom).toHaveBeenLastCalledWith(2);
  });

  /**
   * The echo guard, and the race it closes.
   *
   * The document trails this hook by up to one persist interval, so mid-burst
   * it holds an OLDER level than the one on screen. A sync effect that adopted
   * the stored value unconditionally would pull the level backwards under the
   * user's fingers while they were still holding the key — the zoom would
   * stutter or stick. The guard is what tells "the document caught up with us"
   * apart from "somebody else changed it".
   */
  it("is not dragged backwards by a document that is still catching up", () => {
    const runtime = runtimeState();
    const { result, rerender } = renderHook(({ settings }) => useZoom(runtime, settings), {
      initialProps: { settings: settingsState(1) },
    });

    // Three steps to 1.5, then the first flush writes it.
    act(() => { press("="); press("="); press("="); });
    expect(result.current.level).toBe(1.5);
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });

    // Two more steps while the document still reads the level from the flush
    // above — which is what a re-render carrying that older document looks
    // like.
    act(() => { press("="); press("="); });
    expect(result.current.level).toBe(2);
    rerender({ settings: settingsState(1.5) });
    expect(result.current.level).toBe(2);
    expect(runtime.setZoom).toHaveBeenLastCalledWith(2);
  });

  it("does not bind the keys where the bridge cannot scale a webview", () => {
    const settings = settingsState();
    const runtime = runtimeState({ mode: "fixture" });
    const { result } = renderHook(() => useZoom(runtime, settings));

    expect(result.current.available).toBe(false);
    act(() => press("="));
    // No apply and no write: the browser preview has its own zoom, and a level
    // that persists while doing nothing is worse than one that says so.
    expect(runtime.setZoom).not.toHaveBeenCalled();
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved).toHaveLength(0);
  });

  it("exposes a direct setter for the Settings row, snapped to the ladder", () => {
    const settings = settingsState();
    const runtime = runtimeState();
    const { result } = renderHook(() => useZoom(runtime, settings));

    act(() => result.current.set(2));
    expect(runtime.setZoom).toHaveBeenCalledWith(2);

    act(() => result.current.set(1.3));
    expect(runtime.setZoom).toHaveBeenLastCalledWith(1.25);
  });

  it("reads an off-ladder stored level as the rung it behaves as", () => {
    const settings = settingsState(1.3);
    const { result } = renderHook(() => useZoom(runtimeState(), settings));
    expect(result.current.level).toBe(1.25);
  });

  it("survives a bridge that rejects, because a failed zoom is self-evident", () => {
    const settings = settingsState();
    const runtime = runtimeState({ setZoom: vi.fn(async () => { throw new Error("no webview"); }) });
    renderHook(() => useZoom(runtime, settings));

    expect(() => act(() => press("="))).not.toThrow();
    // The level still advances and is still written: what failed is the scaling,
    // and reverting the user's choice underneath them would be worse than a
    // window that did not resize.
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(settings.saved[0].zoom.level).toBe(1.1);
  });
});
