import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { modeScopedKey } from "../lib/bridge";
import type { DeckPrompt, DeckPromptLibrary } from "../types";
import { PROMPT_SAVE_DELAY_MS, usePromptLibrary, type PromptLibraryAccess } from "./usePromptLibrary";

/**
 * Issue #824 — the prompt library moved from this device's `localStorage` to
 * the selected daemon. These drive the hook directly, because what they pin is
 * where the library comes from and what happens to the copy this device kept —
 * above all that a user's prompts are never lost on the way: the device copy
 * is removed only once a deck has taken it, and kept when it refuses.
 */

const KEY = modeScopedKey("dot-agent-deck.desktop.prompts.v1");

function prompt(id: string, name = id, body = `${id} body`): DeckPrompt {
  return { id, name, body };
}

function storeOnDevice(prompts: DeckPrompt[]) {
  window.localStorage.setItem(KEY, JSON.stringify({ prompts }));
}

function deviceCopy(): DeckPrompt[] | null {
  const raw = window.localStorage.getItem(KEY);
  return raw === null ? null : (JSON.parse(raw) as { prompts: DeckPrompt[] }).prompts;
}

/** A daemon that keeps a library, with the daemon's own seed rule. */
function deckKeeping(initial: DeckPrompt[] = []) {
  let held = [...initial];
  const answer = (): DeckPromptLibrary => ({ kind: "deck", prompts: held.map((entry) => ({ ...entry })) });
  const access = {
    listPrompts: vi.fn(async () => answer()),
    savePrompt: vi.fn(async (_deck: string, saved: DeckPrompt) => {
      held = held.some((entry) => entry.id === saved.id) ? held.map((entry) => (entry.id === saved.id ? saved : entry)) : [...held, saved];
      return answer();
    }),
    deletePrompt: vi.fn(async (_deck: string, id: string) => {
      held = held.filter((entry) => entry.id !== id);
      return answer();
    }),
    seedPrompts: vi.fn(async (_deck: string, seed: DeckPrompt[]) => {
      for (const entry of seed) if (!held.some((existing) => existing.id === entry.id)) held = [...held, entry];
      return answer();
    }),
  } satisfies Omit<PromptLibraryAccess, "deckId">;
  return { access, held: () => held };
}

/** Let queued promise chains run, without depending on timers (some tests fake them). */
async function settle() {
  await act(async () => {
    for (let index = 0; index < 10; index += 1) await Promise.resolve();
  });
}

/** A promise the test settles by hand. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

beforeEach(() => window.localStorage.clear());
afterEach(() => vi.useRealTimers());

describe("usePromptLibrary (issue #824)", () => {
  it("hands the device's library to a deck that keeps prompts, then removes the device copy", async () => {
    storeOnDevice([prompt("a", "Kept on this device"), prompt("b")]);
    const deck = deckKeeping([prompt("a", "Edited on the daemon")]);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));

    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));
    expect(deck.access.seedPrompts).toHaveBeenCalledWith("deck-1", [prompt("a", "Kept on this device"), prompt("b")]);
    // The seed never overwrites what the deck holds.
    expect(result.current.prompts).toEqual([prompt("a", "Edited on the daemon"), prompt("b")]);
    expect(deviceCopy()).toBeNull();
    expect(result.current.notice).toBe("Moved 2 prompts kept on this device into this daemon's library.");
  });

  it("keeps the device copy when the deck refuses the hand-over, and shows the deck's library", async () => {
    storeOnDevice([prompt("a")]);
    const deck = deckKeeping([prompt("z")]);
    deck.access.seedPrompts.mockRejectedValueOnce(new Error("seed-prompts: a prompt body is longer than 65536 bytes"));
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));

    await waitFor(() => expect(result.current.notice).toContain("could not be moved"));
    expect(result.current.notice).toContain("longer than 65536 bytes");
    expect(result.current.source).toEqual({ kind: "deck" });
    expect(result.current.prompts).toEqual([prompt("z")]);
    expect(deviceCopy()).toEqual([prompt("a")]);
  });

  it("keeps the library on this device for a daemon from before the library", async () => {
    storeOnDevice([prompt("a")]);
    const listPrompts = vi.fn(async (): Promise<DeckPromptLibrary> => ({ kind: "unsupported" }));
    const seedPrompts = vi.fn();
    const { result } = renderHook(() => usePromptLibrary({ deckId: "older", listPrompts, seedPrompts }));

    await waitFor(() => expect(result.current.source).toEqual({ kind: "device", reason: "older-deck" }));
    expect(result.current.prompts).toEqual([prompt("a")]);
    expect(seedPrompts).not.toHaveBeenCalled();
    act(() => result.current.updatePrompt("a", { name: "Renamed" }));
    expect(deviceCopy()).toEqual([prompt("a", "Renamed")]);
  });

  it("saves an edit to the deck once typing pauses, and a removal at once", async () => {
    const deck = deckKeeping([prompt("a")]);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));

    vi.useFakeTimers();
    act(() => result.current.updatePrompt("a", { body: "f" }));
    act(() => result.current.updatePrompt("a", { body: "fi" }));
    act(() => result.current.updatePrompt("a", { body: "fix" }));
    expect(result.current.prompts).toEqual([prompt("a", "a", "fix")]);
    expect(deck.access.savePrompt).not.toHaveBeenCalled();
    await act(async () => { vi.advanceTimersByTime(PROMPT_SAVE_DELAY_MS); });
    await settle();
    expect(deck.access.savePrompt).toHaveBeenCalledTimes(1);
    expect(deck.access.savePrompt).toHaveBeenCalledWith("deck-1", prompt("a", "a", "fix"));

    act(() => result.current.removePrompt("a"));
    await settle();
    expect(deck.access.deletePrompt).toHaveBeenCalledWith("deck-1", "a");
    expect(result.current.prompts).toEqual([]);
    // Nothing went back to this device.
    expect(deviceCopy()).toBeNull();
  });

  it("sends a pending edit to the deck it was made on when the selection moves", async () => {
    const deck = deckKeeping([prompt("a")]);
    const { result, rerender } = renderHook(({ deckId }) => usePromptLibrary({ deckId, ...deck.access }), { initialProps: { deckId: "deck-1" as string | undefined } });
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));

    act(() => result.current.updatePrompt("a", { name: "Typed before switching" }));
    rerender({ deckId: "deck-2" });
    await waitFor(() => expect(deck.access.savePrompt).toHaveBeenCalledWith("deck-1", prompt("a", "Typed before switching")));
    await waitFor(() => expect(deck.access.listPrompts).toHaveBeenLastCalledWith("deck-2"));
  });

  it("sends a delete only after the save before it has answered, so a removed prompt stays removed", async () => {
    const deck = deckKeeping([prompt("a")]);
    const slowSave = deferred<DeckPromptLibrary>();
    deck.access.savePrompt.mockImplementationOnce(() => slowSave.promise);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));

    act(() => result.current.updatePrompt("a", { body: "edited" }));
    act(() => result.current.flushPending());
    await settle();
    expect(deck.access.savePrompt).toHaveBeenCalledTimes(1);
    act(() => result.current.removePrompt("a"));
    await settle();
    expect(deck.access.deletePrompt).not.toHaveBeenCalled();
    slowSave.resolve({ kind: "deck", prompts: [prompt("a", "a", "edited")] });
    await settle();
    expect(deck.access.deletePrompt).toHaveBeenCalledWith("deck-1", "a");
  });

  it("sends a new prompt's first edit only after its creation has answered", async () => {
    const deck = deckKeeping();
    const slowCreate = deferred<DeckPromptLibrary>();
    deck.access.savePrompt.mockImplementationOnce(() => slowCreate.promise);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));

    let id: string | undefined;
    act(() => { id = result.current.addPrompt(); });
    act(() => result.current.updatePrompt(id!, { body: "typed" }));
    act(() => result.current.flushPending());
    await settle();
    expect(deck.access.savePrompt).toHaveBeenCalledTimes(1);
    slowCreate.resolve({ kind: "deck", prompts: [] });
    await settle();
    expect(deck.access.savePrompt).toHaveBeenCalledTimes(2);
    expect(deck.access.savePrompt.mock.calls[1]?.[1]).toMatchObject({ id, body: "typed" });
  });

  it("sends a pending edit when the page is hidden", async () => {
    const deck = deckKeeping([prompt("a")]);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));

    act(() => result.current.updatePrompt("a", { name: "Typed just before closing" }));
    act(() => { window.dispatchEvent(new Event("pagehide")); });
    await settle();
    expect(deck.access.savePrompt).toHaveBeenCalledWith("deck-1", prompt("a", "Typed just before closing"));
  });

  it("keeps a device copy edited while the hand-over was in flight", async () => {
    storeOnDevice([prompt("a")]);
    const deck = deckKeeping();
    const slowSeed = deferred<DeckPromptLibrary>();
    deck.access.seedPrompts.mockImplementationOnce(() => slowSeed.promise);
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));
    await waitFor(() => expect(deck.access.seedPrompts).toHaveBeenCalled());

    // Edited on this device (another window, or an older deck) meanwhile.
    storeOnDevice([prompt("a", "edited meanwhile")]);
    slowSeed.resolve({ kind: "deck", prompts: [prompt("a")] });
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));
    expect(deviceCopy()).toEqual([prompt("a", "edited meanwhile")]);
  });

  it("hands the library to one deck at a time, so a quick switch cannot copy it into two", async () => {
    storeOnDevice([prompt("a")]);
    const first = deckKeeping();
    const slowSeed = deferred<DeckPromptLibrary>();
    first.access.seedPrompts.mockImplementationOnce(() => slowSeed.promise);
    const second = deckKeeping();
    const access = (deckId: string | undefined) => ({ deckId, ...(deckId === "deck-2" ? second.access : first.access) });
    const { result, rerender } = renderHook(({ deckId }) => usePromptLibrary(access(deckId)), { initialProps: { deckId: "deck-1" as string | undefined } });
    await waitFor(() => expect(first.access.seedPrompts).toHaveBeenCalled());

    rerender({ deckId: "deck-2" });
    await waitFor(() => expect(result.current.source).toEqual({ kind: "deck" }));
    expect(second.access.seedPrompts).not.toHaveBeenCalled();
    slowSeed.resolve({ kind: "deck", prompts: [prompt("a")] });
    await waitFor(() => expect(deviceCopy()).toBeNull());
  });

  it("moves the prompts a daemon can keep and leaves a too-large one on this device, named", async () => {
    const huge = prompt("big", "Huge context dump", "x".repeat(64 * 1024 + 1));
    storeOnDevice([prompt("a"), huge]);
    const deck = deckKeeping();
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", ...deck.access }));

    await waitFor(() => expect(result.current.notice).toContain("Moved 1 prompt"));
    expect(deck.access.seedPrompts).toHaveBeenCalledWith("deck-1", [prompt("a")]);
    expect(result.current.notice).toContain("Huge context dump");
    expect(result.current.prompts).toEqual([prompt("a")]);
    expect(deviceCopy()).toEqual([huge]);
  });

  it("shows no library and allows no edit with no single daemon selected", () => {
    storeOnDevice([prompt("a")]);
    const deck = deckKeeping();
    const { result } = renderHook(() => usePromptLibrary({ deckId: undefined, ...deck.access }));

    expect(result.current.source).toEqual({ kind: "no-deck" });
    expect(result.current.prompts).toEqual([]);
    act(() => { expect(result.current.addPrompt()).toBeUndefined(); });
    expect(deck.access.listPrompts).not.toHaveBeenCalled();
    // The device copy waits for a deck that keeps prompts.
    expect(deviceCopy()).toEqual([prompt("a")]);
  });

  it("says the daemon could not be read rather than showing an empty library", async () => {
    storeOnDevice([prompt("a")]);
    const listPrompts = vi.fn(async (): Promise<DeckPromptLibrary> => { throw new Error("the daemon took the connection but did not answer ListPrompts within 15s"); });
    const { result } = renderHook(() => usePromptLibrary({ deckId: "deck-1", listPrompts }));

    await waitFor(() => expect(result.current.source.kind).toBe("unavailable"));
    expect(result.current.source).toEqual({ kind: "unavailable", message: "the daemon took the connection but did not answer ListPrompts within 15s" });
    expect(deviceCopy()).toEqual([prompt("a")]);
  });

  it("keeps the old device-only behaviour for a runtime with no deck to ask", () => {
    storeOnDevice([prompt("a")]);
    const { result } = renderHook(() => usePromptLibrary());

    expect(result.current.source).toEqual({ kind: "device", reason: "no-runtime" });
    expect(result.current.prompts).toEqual([prompt("a")]);
    let added: string | undefined;
    act(() => { added = result.current.addPrompt(); });
    expect(deviceCopy()?.map((entry) => entry.id)).toEqual(["a", added]);
  });
});
