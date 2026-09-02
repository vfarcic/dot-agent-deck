import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS, type AppearanceMode, type DesktopSettingsDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";
import { useDesktopSettings } from "./useDesktopSettings";

/**
 * Save ordering (PRD #803).
 *
 * The hook launched every save immediately, so two rapid choices raced twice
 * over: the two writes could reach the disk in either order, and whichever
 * *response* came back last replaced React state. Today that costs a stale
 * appearance choice; it costs a wrong daemon endpoint once #741 lands.
 *
 * These drive the hook directly rather than through the sheet, because the
 * property is about the order of two promises and nothing about what a panel
 * renders. Every `saveSettings` here is deferred by hand — resolved by the test
 * rather than by a timer — so "the second request had not gone out yet" is
 * observable instead of inferred from timing.
 */
function withMode(mode: AppearanceMode): DesktopSettingsDto {
  return { ...DEFAULT_DESKTOP_SETTINGS, appearance: { mode } };
}

/** A `saveSettings` whose every call the test settles by hand. */
function deferredSaves() {
  const sent: DesktopSettingsDto[] = [];
  const pending: { resolve: (written: DesktopSettingsDto) => void; reject: (cause: unknown) => void }[] = [];
  const saveSettings = vi.fn((settings: DesktopSettingsDto) => {
    sent.push(settings);
    return new Promise<DesktopSettingsDto>((resolve, reject) => { pending.push({ resolve, reject }); });
  });
  return { sent, pending, saveSettings };
}

/**
 * The hook reads exactly two members of the runtime, so the rest is not built.
 * The cast is what keeps that honest: a third member would fail to compile here
 * rather than being quietly stubbed.
 */
function runtime(saveSettings: DeckRuntimeState["saveSettings"]): DeckRuntimeState {
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: "/tmp/desktop.toml" })),
    saveSettings,
  } as unknown as DeckRuntimeState;
}

async function loadedHook(saveSettings: DeckRuntimeState["saveSettings"]) {
  const hook = renderHook(() => useDesktopSettings(runtime(saveSettings)));
  await waitFor(() => expect(hook.result.current.loaded).toBe(true));
  return hook;
}

describe("useDesktopSettings save ordering", () => {
  it("sends one save at a time and drops a superseded response", async () => {
    const { sent, pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings);

    // Awaited, because the queue hands each write to a microtask — the first
    // save is enqueued behind an already-resolved promise, not fired inline.
    await act(async () => { result.current.save(withMode("dark")); });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    expect(sent[0].appearance.mode).toBe("dark");

    // The second write waits for the first even after the microtasks drain:
    // two concurrent writes are what let them land in the wrong order.
    act(() => { result.current.save(withMode("light")); });
    await act(async () => { await Promise.resolve(); });
    expect(saveSettings).toHaveBeenCalledTimes(1);
    // The UI, though, already shows the newest choice — the save is optimistic.
    expect(result.current.settings.appearance.mode).toBe("light");

    // The first response echoes `dark`. It is now stale, and applying it would
    // visibly revert a choice the user has already made.
    await act(async () => { pending[0].resolve(withMode("dark")); });
    expect(result.current.settings.appearance.mode).toBe("light");

    // Only now does the second write go out, and its response is the current
    // one, so it is applied.
    expect(saveSettings).toHaveBeenCalledTimes(2);
    expect(sent[1].appearance.mode).toBe("light");
    await act(async () => { pending[1].resolve(withMode("light")); });
    expect(result.current.settings.appearance.mode).toBe("light");
    expect(result.current.saveError).toBeUndefined();
  });

  it("reports the newest save's failure and not a superseded one's", async () => {
    const { pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings);

    await act(async () => { result.current.save(withMode("dark")); });
    act(() => { result.current.save(withMode("light")); });

    // The superseded save fails. Its message would be about a choice that is no
    // longer on screen, so it is not shown — and the chain must carry on.
    await act(async () => { pending[0].reject(new Error("stale disk error")); });
    expect(result.current.saveError).toBeUndefined();
    expect(saveSettings).toHaveBeenCalledTimes(2);

    // The newest one's failure is the one the user needs, and the choice stays
    // applied: what failed is persisting it, not making it.
    await act(async () => { pending[1].reject(new Error("permission denied")); });
    expect(result.current.saveError).toBe("permission denied");
    expect(result.current.settings.appearance.mode).toBe("light");
  });
});
