import { afterEach, describe, expect, it, vi } from "vitest";
import type { SpeechPlanDto } from "./bridge";
import {
  MAX_PROVIDER_AUDIO_SECONDS,
  NO_SYSTEM_VOICE,
  PROVIDER_AUDIO_TOO_LONG,
  SpeechQueue,
  SpeechRefusedError,
  providerPlaybackDeadlineMs,
  providerVoice,
  speechAudioError,
  systemVoice,
  systemVoiceDeadlineMs,
  type AudioContextLike,
  type SpeechVoice,
  type SynthLike,
} from "./speech";

/** A voice whose sentences finish only when the test says so. */
class ScriptedVoice implements SpeechVoice {
  readonly said: string[] = [];
  readonly stopped: string[] = [];
  private finishers: (() => void)[] = [];
  failWith: Error | undefined;

  speak(text: string, signal: AbortSignal): Promise<void> {
    this.said.push(text);
    if (this.failWith) return Promise.reject(this.failWith);
    return new Promise((resolve) => {
      signal.addEventListener("abort", () => {
        this.stopped.push(text);
        resolve();
      });
      this.finishers.push(resolve);
    });
  }

  /** Finish the sentence currently being spoken. */
  finish(): void {
    this.finishers.shift()?.();
  }
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function queueWith(plan: SpeechPlanDto, problems: string[] = []) {
  const provider = new ScriptedVoice();
  const system = new ScriptedVoice();
  const queue = new SpeechQueue({
    plan: () => Promise.resolve(plan),
    provider,
    system,
    onProblem: (reason) => problems.push(reason),
  });
  return { queue, provider, system, problems };
}

describe("SpeechQueue", () => {
  it("speaks with the source the plan names", async () => {
    const onSystem = queueWith({ kind: "system" });
    onSystem.queue.say("tester", "The tester finished: done.");
    await flush();
    expect(onSystem.system.said).toEqual(["The tester finished: done."]);
    expect(onSystem.provider.said).toEqual([]);

    const onProvider = queueWith({ kind: "provider", fallbackToSystem: true });
    onProvider.queue.say("tester", "hello");
    await flush();
    expect(onProvider.provider.said).toEqual(["hello"]);
    expect(onProvider.system.said).toEqual([]);
  });

  it("falls back to the system voice only when the plan allows it", async () => {
    const auto = queueWith({ kind: "provider", fallbackToSystem: true });
    auto.provider.failWith = new Error("no speech route");
    auto.queue.say("tester", "hello");
    await flush();
    expect(auto.system.said).toEqual(["hello"]);
    expect(auto.problems).toEqual([]);

    const chosen = queueWith({ kind: "provider", fallbackToSystem: false });
    chosen.provider.failWith = new Error("no speech route");
    chosen.queue.say("tester", "hello");
    await flush();
    expect(chosen.system.said).toEqual([]);
    expect(chosen.problems).toEqual(["no speech route"]);
    expect(chosen.queue.speaking).toBe(false);
  });

  it("reports an unavailable plan instead of speaking", async () => {
    const { queue, provider, system, problems } = queueWith({ kind: "unavailable", reason: "no text-to-speech" });
    queue.say("tester", "hello");
    await flush();
    expect(provider.said).toEqual([]);
    expect(system.said).toEqual([]);
    expect(problems).toEqual(["no text-to-speech"]);
    expect(queue.speaking).toBe(false);
  });

  it("replaces a waiting sentence for the same agent instead of piling up", async () => {
    const { queue, system } = queueWith({ kind: "system" });
    queue.say("tester", "first");
    await flush();
    queue.say("tester", "second");
    queue.say("coder", "coder news");
    queue.say("tester", "third");
    // "first" is being spoken and is left alone; "second" was replaced.
    expect(queue.pending).toEqual([
      { agent: "tester", text: "third" },
      { agent: "coder", text: "coder news" },
    ]);
    system.finish();
    await flush();
    system.finish();
    await flush();
    system.finish();
    await flush();
    expect(system.said).toEqual(["first", "third", "coder news"]);
    expect(queue.speaking).toBe(false);
  });

  it("interrupt stops the current sentence at once and drops what is waiting", async () => {
    const { queue, system } = queueWith({ kind: "system" });
    queue.say("tester", "first");
    queue.say("coder", "second");
    await flush();
    expect(queue.speaking).toBe(true);
    queue.interrupt();
    await flush();
    expect(system.stopped).toEqual(["first"]);
    expect(system.said).toEqual(["first"]);
    expect(queue.pending).toEqual([]);
    expect(queue.speaking).toBe(false);
  });

  it("interrupt ends the sentence even when the voice ignores its signal", async () => {
    const deaf: SpeechVoice = { speak: () => new Promise(() => undefined) };
    const queue = new SpeechQueue({ plan: () => Promise.resolve({ kind: "system" }), provider: deaf, system: deaf });
    queue.say("tester", "hello");
    await flush();
    expect(queue.speaking).toBe(true);
    queue.interrupt();
    await flush();
    expect(queue.speaking).toBe(false);
  });

  it("speaks again after an interrupt", async () => {
    const { queue, system } = queueWith({ kind: "system" });
    queue.say("tester", "first");
    await flush();
    queue.interrupt();
    queue.say("tester", "after");
    await flush();
    expect(system.said).toEqual(["first", "after"]);
    expect(queue.speaking).toBe(true);
  });

  it("reports is-speaking transitions once per busy period, without flicker", async () => {
    const { queue, system } = queueWith({ kind: "system" });
    const seen: boolean[] = [];
    queue.subscribe((speaking) => seen.push(speaking));
    expect(queue.speaking).toBe(false);
    queue.say("tester", "one");
    queue.say("coder", "two");
    expect(queue.speaking).toBe(true);
    await flush();
    system.finish();
    await flush();
    expect(queue.speaking).toBe(true);
    system.finish();
    await flush();
    expect(queue.speaking).toBe(false);
    expect(seen).toEqual([true, false]);
  });

  it("ignores empty text", () => {
    const { queue } = queueWith({ kind: "system" });
    queue.say("tester", "   ");
    expect(queue.speaking).toBe(false);
  });
});

describe("systemVoice", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  /** A synth that records utterances; the test fires their events. */
  function fakeSynth() {
    const spoken: SpeechSynthesisUtterance[] = [];
    const synth: SynthLike & { cancels: number } = {
      cancels: 0,
      speak: (utterance) => spoken.push(utterance),
      cancel() {
        this.cancels += 1;
      },
    };
    const utter = (text: string) => ({ text, onend: null, onerror: null }) as unknown as SpeechSynthesisUtterance;
    return { synth, spoken, utter };
  }

  it("rejects when the webview has no speech synthesis", async () => {
    await expect(systemVoice(undefined, undefined).speak("hi", new AbortController().signal)).rejects.toThrow(NO_SYSTEM_VOICE);
  });

  it("resolves when the utterance ends, and cancels on abort", async () => {
    const { synth, spoken, utter } = fakeSynth();
    const voice = systemVoice(synth, utter);
    const done = voice.speak("hello", new AbortController().signal);
    (spoken[0].onend as () => void)();
    await expect(done).resolves.toBeUndefined();

    const controller = new AbortController();
    const stopped = voice.speak("again", controller.signal);
    controller.abort();
    await expect(stopped).resolves.toBeUndefined();
    expect(synth.cancels).toBe(1);
  });

  it("gives up on a voice that never reports the end", async () => {
    vi.useFakeTimers();
    const { synth, utter } = fakeSynth();
    const done = systemVoice(synth, utter).speak("hello", new AbortController().signal);
    vi.advanceTimersByTime(systemVoiceDeadlineMs("hello"));
    await expect(done).resolves.toBeUndefined();
    expect(synth.cancels).toBe(1);
  });
});

describe("providerVoice", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  function fakeContext(duration?: number) {
    const started: string[] = [];
    const source = {
      buffer: null as AudioBuffer | null,
      onended: null as (() => void) | null,
      connect: vi.fn(),
      start: () => started.push("start"),
      stop: () => started.push("stop"),
    };
    const context: AudioContextLike = {
      destination: {} as AudioNode,
      resume: () => Promise.resolve(),
      decodeAudioData: () => Promise.resolve((duration === undefined ? {} : { duration }) as AudioBuffer),
      createBufferSource: () => source as unknown as AudioBufferSourceNode,
    };
    return { context, source, started };
  }

  it("plays the fetched audio and stops it on abort", async () => {
    const { context, source, started } = fakeContext();
    const fetched: string[] = [];
    const voice = providerVoice((text) => {
      fetched.push(text);
      return Promise.resolve(new ArrayBuffer(4));
    }, () => context);

    const done = voice.speak("hello", new AbortController().signal);
    await flush();
    expect(fetched).toEqual(["hello"]);
    expect(started).toEqual(["start"]);
    source.onended?.();
    await expect(done).resolves.toBeUndefined();

    const controller = new AbortController();
    const stopped = voice.speak("again", controller.signal);
    await flush();
    controller.abort();
    await expect(stopped).resolves.toBeUndefined();
    expect(started).toEqual(["start", "start", "stop"]);
  });

  /** Scenario (audit A-S2): decoded provider audio longer than the cap is refused before anything plays. */
  it("refuses decoded audio longer than the cap", async () => {
    const { context, started } = fakeContext(MAX_PROVIDER_AUDIO_SECONDS + 1);
    const voice = providerVoice(() => Promise.resolve(new ArrayBuffer(4)), () => context);
    await expect(voice.speak("hello", new AbortController().signal)).rejects.toThrow(PROVIDER_AUDIO_TOO_LONG);
    expect(started).toEqual([]);
  });

  /** Scenario (audit A-S2): a source that never reports its end is stopped at the playback deadline, so it cannot hold the speech queue. */
  it("stops playback that never reports its end at the deadline", async () => {
    vi.useFakeTimers();
    const { context, started } = fakeContext(3);
    const voice = providerVoice(() => Promise.resolve(new ArrayBuffer(4)), () => context);
    const done = voice.speak("hello", new AbortController().signal);
    await vi.advanceTimersByTimeAsync(0);
    expect(started).toEqual(["start"]);
    await vi.advanceTimersByTimeAsync(providerPlaybackDeadlineMs(3) - 1);
    expect(started).toEqual(["start"]);
    await vi.advanceTimersByTimeAsync(1);
    await expect(done).resolves.toBeUndefined();
    expect(started).toEqual(["start", "stop"]);
    // A length the decoder did not report is given the cap.
    expect(providerPlaybackDeadlineMs(Number.NaN)).toBe(providerPlaybackDeadlineMs(MAX_PROVIDER_AUDIO_SECONDS));
  });

  it("rejects when the audio cannot be fetched", async () => {
    const voice = providerVoice(() => Promise.reject(new Error("no key")), () => fakeContext().context);
    await expect(voice.speak("hello", new AbortController().signal)).rejects.toThrow("no key");
  });
});

describe("speechAudioError", () => {
  /** Scenario (PR #1617 round 3): the speech command's rejection names a refusal apart from a failure; a refusal becomes a SpeechRefusedError, which Auto never answers with the system voice, and anything else a plain error carrying its sentence. */
  it("tells a refusal apart from a failure", () => {
    const refused = speechAudioError({ kind: "refused", message: "not permitted" });
    expect(refused).toBeInstanceOf(SpeechRefusedError);
    expect(refused.message).toBe("not permitted");
    const failed = speechAudioError({ kind: "failed", message: "the speech request failed" });
    expect(failed).not.toBeInstanceOf(SpeechRefusedError);
    expect(failed.message).toBe("the speech request failed");
    expect(speechAudioError("plain").message).toBe("plain");
    expect(speechAudioError(undefined).message).toBe("speech failed");
  });
});
