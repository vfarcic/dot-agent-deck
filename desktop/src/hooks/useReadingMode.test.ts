import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { READING_NOT_IN_THIS_RUNTIME, useReadingMode } from "./useReadingMode";
import { READING_ON, type ReadingAgent } from "../lib/reading";
import { wordedNow, type SpeechText } from "../lib/speech";

const PLANNER: ReadingAgent = { deckId: "deck-00000000000000a1", agentId: "planner", label: "Plan / architecture" };

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/**
 * PRD #1497 (PR #1617's fifth review) — reading starts in a window only once
 * that window can hear a save turn reading's switch off. Driven through the
 * hook because the ordering lives in it: the reader itself knows nothing of
 * the listener.
 */
describe("useReadingMode", () => {
  function mount(register: (listener: () => void) => Promise<() => void>) {
    const consentOn = new Set<() => void>();
    const runtime = {
      voiceSpeechPlan: vi.fn(async () => ({ kind: "system" as const })),
      voiceSpeechAudio: vi.fn(async () => new ArrayBuffer(0)),
      voiceReadingStart: vi.fn(async () => ({ kind: "started" as const, session: 7 })),
      voiceReadingStop: vi.fn(async () => undefined),
      onVoiceReadingConsentOff: vi.fn(register),
      onVoiceReadingConsentOn: vi.fn(async (listener: () => void) => {
        consentOn.add(listener);
        return () => { consentOn.delete(listener); };
      }),
    };
    const problems: string[] = [];
    const hook = renderHook(() => useReadingMode(runtime, () => undefined, undefined, (sentence) => problems.push(sentence)));
    const said: string[] = [];
    vi.spyOn(hook.result.current.queue, "say").mockImplementation((_key, text: SpeechText) => { said.push(wordedNow(text)); });
    return { runtime, hook, said, problems, consentOn };
  }

  /** Scenario: the consent-off listener is still registering when Reading is turned on; no agent is subscribed until the registration settles, then the deck's agent is read as usual. */
  it("waits for the consent-off listener before reading starts", async () => {
    let installed!: (stop: () => void) => void;
    const { runtime, hook, said } = mount(() => new Promise((resolve) => { installed = resolve; }));
    await act(async () => {
      hook.result.current.reader.update(true, [PLANNER]);
      await flush();
    });
    expect(runtime.voiceReadingStart).not.toHaveBeenCalled();
    expect(said).toEqual([READING_ON]);
    await act(async () => {
      installed(() => undefined);
      await flush();
    });
    expect(runtime.voiceReadingStart).toHaveBeenCalledTimes(1);
  });

  /** Scenario: the consent-off listener fails to register; reading never subscribes any agent, and says once that it is not available. */
  it("reads nothing when the consent-off listener could not be installed", async () => {
    const { runtime, hook, problems } = mount(async () => { throw new Error("event API unavailable"); });
    await act(async () => {
      hook.result.current.reader.update(true, [PLANNER, { ...PLANNER, agentId: "coder", label: "coder" }]);
      await flush();
    });
    expect(runtime.voiceReadingStart).not.toHaveBeenCalled();
    expect(problems).toEqual([READING_NOT_IN_THIS_RUNTIME]);
  });

  /** Scenario (audit A1): the consent-on listener is still registering when Reading is turned on; no agent is subscribed until it is in place, so a save reported on after a start read the settings cannot be missed. */
  it("waits for the consent-on listener before reading starts", async () => {
    const { runtime, hook } = mount(async () => () => undefined);
    await act(async () => { await flush(); });
    let installed!: (stop: () => void) => void;
    runtime.onVoiceReadingConsentOn.mockImplementationOnce(() => new Promise((resolve) => { installed = resolve; }));
    hook.unmount();
    const again = renderHook(() => useReadingMode(runtime, () => undefined));
    await act(async () => {
      again.result.current.reader.update(true, [PLANNER]);
      await flush();
    });
    expect(runtime.voiceReadingStart).not.toHaveBeenCalled();
    await act(async () => {
      installed(() => undefined);
      await flush();
    });
    expect(runtime.voiceReadingStart).toHaveBeenCalledTimes(1);
    again.unmount();
  });

  /** Scenario: a save left Reading on; the consent-on event reaches the reader, which retries a start refused before that save reached the disk. */
  it("retries a refused start when a save reports Reading on", async () => {
    const { runtime, hook, consentOn } = mount(async () => () => undefined);
    runtime.voiceReadingStart.mockResolvedValueOnce({ kind: "not_enabled", sentence: "Reading is off." } as never);
    await act(async () => {
      hook.result.current.reader.update(true, [PLANNER]);
      await flush();
    });
    expect(runtime.voiceReadingStart).toHaveBeenCalledTimes(1);
    expect(consentOn.size).toBe(1);
    await act(async () => {
      for (const listener of consentOn) listener();
      await flush();
    });
    expect(runtime.voiceReadingStart).toHaveBeenCalledTimes(2);
  });
});
