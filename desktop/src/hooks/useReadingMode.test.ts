import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useReadingMode } from "./useReadingMode";
import { READING_ON, READING_START_FAILED, type ReadingTarget } from "../lib/reading";

const TARGET: ReadingTarget = { deckId: "deck-00000000000000a1", agentId: "planner", label: "Plan / architecture" };

/**
 * PRD #1497 (PR #1617's fifth review) — reading starts in a window only once
 * that window can hear another window turn reading's opt-in off. Driven through
 * the hook because the ordering lives in it: the mode itself knows nothing of
 * the listener.
 */
describe("useReadingMode", () => {
  function mount(register: (listener: () => void) => Promise<() => void>) {
    const runtime = {
      voiceSpeechPlan: vi.fn(async () => ({ kind: "system" as const })),
      voiceSpeechAudio: vi.fn(async () => new ArrayBuffer(0)),
      voiceReadingStart: vi.fn(async () => ({ kind: "started" as const, session: 7 })),
      voiceReadingStop: vi.fn(async () => undefined),
      onVoiceReadingConsentOff: vi.fn(register),
    };
    const hook = renderHook(() => useReadingMode(runtime));
    const said: string[] = [];
    vi.spyOn(hook.result.current.queue, "say").mockImplementation((_key, text) => { said.push(text); });
    return { runtime, hook, said };
  }

  /** Scenario: the consent-off listener is still registering when "reading on" is said; reading does not start until the registration settles, then starts as usual. */
  it("waits for the consent-off listener before reading starts", async () => {
    let installed!: (stop: () => void) => void;
    const { runtime, hook, said } = mount(() => new Promise((resolve) => { installed = resolve; }));
    let turned: Promise<unknown> | undefined;
    await act(async () => {
      turned = hook.result.current.mode.turnOn(TARGET);
      await Promise.resolve();
    });
    expect(runtime.voiceReadingStart).not.toHaveBeenCalled();
    expect(said).toEqual([]);
    await act(async () => {
      installed(() => undefined);
      await turned;
    });
    expect(runtime.voiceReadingStart).toHaveBeenCalledTimes(1);
    expect(said).toEqual([READING_ON]);
  });

  /** Scenario: the consent-off listener fails to register; "reading on" is refused with the plain start-failed sentence, the reading start is never asked for, and nothing else is spoken. */
  it("refuses to read when the consent-off listener could not be installed", async () => {
    const { runtime, hook, said } = mount(async () => { throw new Error("event API unavailable"); });
    let answer: unknown;
    await act(async () => {
      answer = await hook.result.current.mode.turnOn(TARGET);
    });
    expect(answer).toEqual({ kind: "refused", sentence: READING_START_FAILED });
    expect(runtime.voiceReadingStart).not.toHaveBeenCalled();
    expect(said).toEqual([READING_START_FAILED]);
    expect(hook.result.current.mode.on).toBe(false);
  });
});
