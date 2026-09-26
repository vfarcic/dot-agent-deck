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
  const bases: (DesktopSettingsDto | undefined)[] = [];
  const pending: { resolve: (written: DesktopSettingsDto) => void; reject: (cause: unknown) => void }[] = [];
  const saveSettings = vi.fn((settings: DesktopSettingsDto, base?: DesktopSettingsDto) => {
    sent.push(settings);
    bases.push(base);
    return new Promise<DesktopSettingsDto>((resolve, reject) => { pending.push({ resolve, reject }); });
  });
  return { sent, bases, pending, saveSettings };
}

/**
 * The hook reads exactly two members of the runtime, so the rest is not built.
 * The cast is what keeps that honest: a third member would fail to compile here
 * rather than being quietly stubbed.
 */
function runtime(saveSettings: DeckRuntimeState["saveSettings"], problem?: string): DeckRuntimeState {
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: "/tmp/desktop.toml", problem })),
    saveSettings,
  } as unknown as DeckRuntimeState;
}

async function loadedHook(saveSettings: DeckRuntimeState["saveSettings"], problem?: string) {
  // Built ONCE, outside the render callback. `useDesktopSettings` keys its load
  // effect on `getSettings`, and the real runtime memoises that with
  // `useCallback` — building a fresh runtime per render instead would re-run the
  // read on every state change and quietly re-seed whatever the read reports,
  // which is a property of this helper and not of the hook.
  const value = runtime(saveSettings, problem);
  const hook = renderHook(() => useDesktopSettings(value));
  await waitFor(() => expect(hook.result.current.loaded).toBe(true));
  return hook;
}

/** The sentence the Rust side sends when the document on disk cannot be read. */
const UNREADABLE = "The desktop settings file cannot be read: line 3, column 9 is not valid settings. This session is using default settings, and nothing will be saved over the file until it is fixed or removed.";

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
    expect(result.current.saveError).toContain("permission denied");
    // The lead-in the panels used to compose themselves now lives here, because
    // the same slot also carries an unreadable-document message (issue #1072).
    expect(result.current.saveError).toContain("will not survive a restart");
    expect(result.current.settings.appearance.mode).toBe("light");
  });
});

/**
 * Two writers (issue #828).
 *
 * The Rust side writes only what differs between a save's `base` and its
 * document, so the base must be exactly what the edit was made against: the
 * document on screen at the click. Diffing against anything newer would read
 * this window's stale copy of another window's field as an edit and write it
 * back over the newer value — the lost edit #828 is about.
 */
describe("useDesktopSettings base", () => {
  it("sends the document each edit was made against, and shows the file as written", async () => {
    const { sent, bases, pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings);

    await act(async () => { result.current.save(withMode("dark")); });
    expect(bases[0]).toEqual(DEFAULT_DESKTOP_SETTINGS);
    expect(sent[0].appearance.mode).toBe("dark");

    // The reply is the file as written, and another window had changed the
    // zoom meanwhile. The window must show that, not its own stale zoom.
    const written = { ...withMode("dark"), zoom: { level: 1.5 } };
    await act(async () => { pending[0].resolve(written); });
    expect(result.current.settings.zoom.level).toBe(1.5);

    // The next edit is made against what is now on screen.
    await act(async () => { result.current.save({ ...result.current.settings, appearance: { mode: "light" } }); });
    expect(bases[1]).toEqual(written);
    expect(sent[1]).toEqual({ ...written, appearance: { mode: "light" } });
  });

  it("captures the base at the click, not when a queued save goes out", async () => {
    const { bases, pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings);

    await act(async () => { result.current.save(withMode("dark")); });
    // A second choice while the first is still in flight: it was made against
    // the dark document on screen, whatever the first reply later says.
    act(() => { result.current.save(withMode("light")); });

    // The first reply carries another window's zoom. Had the queued save taken
    // its base from here, its own zoom — still the default — would read as an
    // edit and be written back over the other window's.
    await act(async () => { pending[0].resolve({ ...withMode("dark"), zoom: { level: 1.5 } }); });
    expect(saveSettings).toHaveBeenCalledTimes(2);
    expect(bases[1]).toEqual(withMode("dark"));
  });
});

/**
 * An unreadable `desktop.toml` (issue #1072).
 *
 * The Rust side comes up on defaults and refuses every save, so the app looks
 * exactly like a fresh install. Nothing but this message explains why — and
 * before #1072 there was no message: the reason went to stderr and the next save
 * overwrote the user's file with the defaults on screen.
 */
describe("useDesktopSettings unreadable document", () => {
  it("surfaces the reason as soon as the load resolves, before any save", async () => {
    const { saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings, UNREADABLE);

    expect(result.current.saveError).toBe(UNREADABLE);
    expect(saveSettings).not.toHaveBeenCalled();
    // The document shown is this build's defaults, which is what the Rust side
    // sent — the hook does not invent one.
    expect(result.current.settings).toEqual(DEFAULT_DESKTOP_SETTINGS);
  });

  it("keeps the reason visible across the optimistic clear a save begins with", async () => {
    const { pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings, UNREADABLE);

    // The click clears the last SAVE failure, and must not clear this: the
    // message would otherwise blink out at the one moment the user is reading
    // it, and come back when the refusal lands a round trip later.
    await act(async () => { result.current.save(withMode("dark")); });
    expect(result.current.saveError).toBe(UNREADABLE);

    // The refusal arrives and wins, because it is the newer and more specific
    // answer — and it still says the change is applied but unsaved.
    await act(async () => { pending[0].reject(new Error("refusing to overwrite the desktop settings file: line 3, column 9 is not valid settings")); });
    expect(result.current.saveError).toContain("refusing to overwrite");
    expect(result.current.settings.appearance.mode).toBe("dark");
  });

  it("stops reporting it once a save gets through", async () => {
    const { pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings, UNREADABLE);

    // A save that comes back at all means `save_to` got past its guard, so the
    // document on disk is readable again — the user fixed or removed the file.
    await act(async () => { result.current.save(withMode("dark")); });
    await act(async () => { pending[0].resolve(withMode("dark")); });

    expect(result.current.saveError).toBeUndefined();
    expect(result.current.problem).toBeUndefined();
  });

  it("exposes the load problem on its own, unchanged by a save that fails", async () => {
    const { pending, saveSettings } = deferredSaves();
    const { result } = await loadedHook(saveSettings, UNREADABLE);
    expect(result.current.problem).toBe(UNREADABLE);

    // A failed save replaces `saveError`, but the footer's `problem` is about
    // the file and must still say what is wrong with it (issue #829).
    await act(async () => { result.current.save(withMode("dark")); });
    await act(async () => { pending[0].reject(new Error("Permission denied")); });
    expect(result.current.saveError).toContain("Permission denied");
    expect(result.current.problem).toBe(UNREADABLE);
  });

  it("does not let a load that resolves after an accepted save restore its stale problem", async () => {
    // The read is still in flight when the user saves, and the backend accepts
    // the save — the file was fixed in between. The read's problem describes the
    // file as it was before that write, so it must not reach the footer.
    const { pending, saveSettings } = deferredSaves();
    let finishLoad: (snapshot: { settings: DesktopSettingsDto; path: string; problem?: string }) => void = () => undefined;
    const value = {
      getSettings: vi.fn(() => new Promise((resolve) => { finishLoad = resolve; })),
      saveSettings,
    } as unknown as DeckRuntimeState;
    const { result } = renderHook(() => useDesktopSettings(value));

    await act(async () => { result.current.save(withMode("dark")); });
    await act(async () => { pending[0].resolve(withMode("dark")); });
    await act(async () => { finishLoad({ settings: structuredClone(DEFAULT_DESKTOP_SETTINGS), path: "/tmp/desktop.toml", problem: UNREADABLE }); });

    expect(result.current.loaded).toBe(true);
    expect(result.current.path).toBe("/tmp/desktop.toml");
    expect(result.current.problem).toBeUndefined();
    expect(result.current.saveError).toBeUndefined();
  });

  it("still reports the problem a load that starts after an accepted save finds", async () => {
    // Only a read already in flight when the save landed is stale. A later read
    // — `getSettings` changes identity when the bridge does — describes the file
    // as it is now, and a document broken again since must reach the footer.
    const { pending, saveSettings } = deferredSaves();
    const first = runtime(saveSettings);
    const { result, rerender } = renderHook(({ value }) => useDesktopSettings(value), { initialProps: { value: first } });
    await waitFor(() => expect(result.current.loaded).toBe(true));

    await act(async () => { result.current.save(withMode("dark")); });
    await act(async () => { pending[0].resolve(withMode("dark")); });
    expect(result.current.problem).toBeUndefined();

    rerender({ value: runtime(saveSettings, UNREADABLE) });
    await waitFor(() => expect(result.current.problem).toBe(UNREADABLE));
  });
});
