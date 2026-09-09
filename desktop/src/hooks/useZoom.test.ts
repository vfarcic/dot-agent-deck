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
/**
 * A settings state that behaves like the real `useDesktopSettings`, which
 * matters more than it sounds.
 *
 * `save` there is **optimistic**: it sets React state to the new document
 * synchronously and writes behind it. So after a write, the document the hook
 * reads back already carries the new level. An earlier version of this harness
 * only recorded the call and left `settings` alone, which manufactured a lag
 * that cannot happen in the app — and hid a spurious adopt in `useZoom`'s sync
 * effect behind test-only timing. Mutating in place is enough, because every
 * write is accompanied by the `setLevel` that re-renders.
 */
function settingsState(level = 1, overrides: Partial<DesktopSettingsState> = {}): DesktopSettingsState & { saved: DesktopSettingsDto[] } {
  const saved: DesktopSettingsDto[] = [];
  const state: DesktopSettingsState & { saved: DesktopSettingsDto[] } = {
    settings: { ...DEFAULT_DESKTOP_SETTINGS, zoom: { level } },
    loaded: true,
    save: vi.fn((next: DesktopSettingsDto) => {
      saved.push(next);
      state.settings = next;
    }),
    saved,
    ...overrides,
  };
  return state;
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
    // Leading edge: the FIRST change of the burst is on disk already, and the
    // other five are collapsed rather than written. Six keystrokes, one write —
    // the property is that a held key does not rewrite desktop.toml once per
    // key repeat, not that it writes nothing.
    expect(settings.saved).toHaveLength(1);
    expect(settings.saved[0].zoom.level).toBe(0.9);

    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    // 0.75 stepped six rungs is 1.75, and the trailing flush writes exactly
    // that — the four intermediate levels are dropped, not queued.
    expect(settings.saved).toHaveLength(2);
    expect(settings.saved[1].zoom.level).toBe(1.75);
  });

  /**
   * Why the leading write exists, beyond matching this app's own coalescer.
   *
   * `useDesktopSettings` protects a user's choice from the asynchronous initial
   * load with an `edited` ref — but it sets that ref only *inside* `save`. With
   * a trailing-only flush, `save` has not been called yet when `getSettings()`
   * resolves, so a zoom keystroke made in that window was overwritten by
   * whatever was on disk at launch: the change appeared to take and then
   * reverted a moment later.
   *
   * The `edited` ref lives in the real hook, so what this can assert is the
   * half that closes the race here: the keystroke reaches `save` **before** any
   * timer runs, which is what gets the ref set in time.
   */
  it("writes the first change of a burst before any timer runs", () => {
    const settings = settingsState();
    renderHook(() => useZoom(runtimeState(), settings));

    act(() => press("="));
    expect(settings.save).toHaveBeenCalledTimes(1);
    expect(settings.saved[0].zoom.level).toBe(1.1);
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

    // Three windows of a sustained hold. Each writes, so the level on disk
    // tracks the one on screen throughout — a pure debounce would have written
    // nothing at all until the key came up, losing the level entirely if the
    // app died mid-burst.
    for (let window = 0; window < 3; window += 1) {
      act(() => { press("="); press("="); });
      act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    }
    expect(settings.saved.length).toBeGreaterThanOrEqual(3);
    // 0.75 stepped six rungs, and the last thing written is where it ended up.
    expect(settings.saved[settings.saved.length - 1].zoom.level).toBe(1.75);
  });

  it("flushes a pending level on unmount, so the last change of a session survives", () => {
    const settings = settingsState();
    const { unmount } = renderHook(() => useZoom(runtimeState(), settings));

    // Two presses: the first is the leading write, the second is only pending.
    act(() => { press("="); press("="); });
    expect(settings.saved).toHaveLength(1);
    expect(settings.saved[0].zoom.level).toBe(1.1);

    unmount();
    expect(settings.saved).toHaveLength(2);
    expect(settings.saved[1].zoom.level).toBe(1.25);
  });

  it("writes nothing extra on unmount when nothing is pending", () => {
    const settings = settingsState();
    const { unmount } = renderHook(() => useZoom(runtimeState(), settings));

    act(() => press("="));
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    const before = settings.saved.length;
    unmount();
    expect(settings.saved).toHaveLength(before);
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
   * The echo guard, and why it is load-bearing rather than defensive.
   *
   * `useDesktopSettings.save` is optimistic, so the leading write of a burst
   * makes the stored document read back the FIRST level of that burst while the
   * screen is already several rungs further on. A sync effect that adopted the
   * stored value on every change would therefore drag the level backwards
   * mid-burst — 1.1 written, 1.5 on screen, and the effect resets it to 1.1.
   * Comparing against `lastPersisted` is what tells "the document caught up
   * with us" apart from "somebody else changed it".
   *
   * An earlier version of this test built the scenario by re-rendering with a
   * deliberately stale settings object. That was unreachable in the app — an
   * optimistic save means the document never lags — and it was only passing
   * because the harness's `save` did not update `settings` either. The burst
   * below is the real mechanism.
   */
  it("is not dragged backwards by its own leading write", () => {
    const settings = settingsState();
    const { result } = renderHook(() => useZoom(runtimeState(), settings));

    act(() => { press("="); press("="); press("="); });
    // The document holds the leading write…
    expect(settings.saved[0].zoom.level).toBe(1.1);
    // …and the level is where the third keystroke put it, not back at the
    // document's value.
    expect(result.current.level).toBe(1.5);

    // Still true after the trailing flush brings the document level with it.
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS); });
    expect(result.current.level).toBe(1.5);
    act(() => press("="));
    expect(result.current.level).toBe(1.75);
  });

  /**
   * Greptile P1 on PR #880, and neither fix was covered by the suite that
   * shipped them — so these are the two tests that would have caught them.
   *
   * A keyboard burst arms a timer holding a pending level. If the user then
   * picks a level in Settings, that pending value is stale: leaving it armed
   * writes the OLDER keyboard level over the newer choice 400ms later, and
   * because the write moves `lastPersisted`, the next render adopts backwards
   * and the window visibly falls back too. The wrong level is then restored at
   * the next launch.
   */
  it("drops a pending keyboard write when the document changes underneath it", () => {
    const settings = settingsState();
    const { result, rerender } = renderHook(({ state }) => useZoom(runtimeState(), state), {
      initialProps: { state: settings },
    });

    // A burst: 1.1 written on the leading edge, 1.25 left pending.
    act(() => { press("="); press("="); });
    expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1]);

    // The Settings row picks 2.0 — its only channel is the document.
    act(() => { settings.save({ ...settings.settings, zoom: { level: 2 } }); });
    rerender({ state: settings });
    expect(result.current.level).toBe(2);

    // The stale pending 1.25 must never land.
    act(() => { vi.advanceTimersByTime(ZOOM_PERSIST_INTERVAL_MS * 2); });
    expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1, 2]);
    expect(settings.settings.zoom.level).toBe(2);
    // And the window did not fall back either.
    expect(result.current.level).toBe(2);
  });

  /**
   * Closing the native window tears the webview down without unmounting
   * anything, so the React cleanup alone did not save a burst's trailing
   * value. `pagehide` and a `visibilitychange` to hidden are the teardown
   * signals a webview actually gives us.
   */
  it("flushes a pending level on pagehide", () => {
    const settings = settingsState();
    const { unmount } = renderHook(() => useZoom(runtimeState(), settings));
    act(() => { press("="); press("="); });
    expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1]);

    act(() => { window.dispatchEvent(new Event("pagehide")); });
    expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1, 1.25]);
    unmount();
  });

  it("flushes a pending level when the window goes hidden", () => {
    const settings = settingsState();
    const { unmount } = renderHook(() => useZoom(runtimeState(), settings));
    act(() => { press("="); press("="); });

    const spy = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    try {
      act(() => { document.dispatchEvent(new Event("visibilitychange")); });
      expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1, 1.25]);
    } finally {
      spy.mockRestore();
      unmount();
    }
  });

  // `visibilitychange` fires on becoming visible too, and a flush there would
  // be harmless but pointless — asserted so the listener stays keyed on the
  // hidden transition rather than on every change.
  it("does not flush when the window merely becomes visible again", () => {
    const settings = settingsState();
    renderHook(() => useZoom(runtimeState(), settings));
    act(() => { press("="); press("="); });

    const spy = vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    try {
      act(() => { document.dispatchEvent(new Event("visibilitychange")); });
      expect(settings.saved.map((doc) => doc.zoom.level)).toEqual([1.1]);
    } finally {
      spy.mockRestore();
    }
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
