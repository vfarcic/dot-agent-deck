/**
 * PRD #1497 M4 — reading mode's voice: one queue, two sources.
 *
 * Reading mode hands {@link SpeechQueue.say} a sentence for an agent; the
 * queue speaks it with whichever source `desktop_voice_speech_plan` names
 * (D9): the Commands connection's text-to-speech, fetched Rust-side and played
 * here through Web Audio, or the operating system's voice through the Web
 * Speech API. Both play in the webview, which is why the queue lives here.
 *
 * # Quiet by default (D6)
 *
 * New speech for an agent that already has a sentence WAITING replaces that
 * sentence rather than queueing behind it, so a burst of events is heard as
 * its latest state instead of as a backlog. A sentence already being spoken is
 * left to finish. {@link SpeechQueue.interrupt} stops the current sentence at
 * once and drops everything waiting — what "stop" and "quiet" do.
 *
 * # "Is speaking" (D8)
 *
 * {@link SpeechQueue.speaking} is true from the moment a sentence is taken off
 * the queue — including while its provider audio is being fetched — until the
 * queue is empty, and does not flicker between back-to-back sentences.
 * Reading mode uses it to honour only the interrupt rows while the app is
 * talking, so the microphone hearing the app's own voice can at worst make it
 * stop itself.
 *
 * # The system voice on Linux
 *
 * WebKitGTK implements `speechSynthesis` only when it was built with speech
 * synthesis support and the desktop has a speech provider installed; without
 * one it may be absent or may accept an utterance and say nothing. The system
 * voice here therefore rejects when the API is missing and gives up on an
 * utterance after a length-based deadline rather than waiting forever, and the
 * provider voice (an OpenAI-compatible connection) is what works regardless.
 */

import type { DeckBridge, SpeechPlanDto } from "./bridge";

/** One way of saying a sentence. Resolves when it has been said or stopped. */
export interface SpeechVoice {
  speak(text: string, signal: AbortSignal): Promise<void>;
}

export interface SpeechQueueDeps {
  /** Which source speaks the next sentence; asked once per sentence. */
  plan: () => Promise<SpeechPlanDto>;
  provider: SpeechVoice;
  system: SpeechVoice;
  /** A sentence could not be spoken, and why. */
  onProblem?: (reason: string) => void;
}

interface Waiting {
  agent: string;
  text: string;
}

/** Said when the webview has no speech synthesis at all. */
export const NO_SYSTEM_VOICE = "this system has no speech voice";

export class SpeechQueue {
  private waiting: Waiting[] = [];
  private current: AbortController | null = null;
  private active = false;
  private readonly listeners = new Set<(speaking: boolean) => void>();

  constructor(private readonly deps: SpeechQueueDeps) {}

  /** Whether a sentence is being prepared or spoken. */
  get speaking(): boolean {
    return this.active;
  }

  /** The sentences waiting, oldest first (not the one being spoken). */
  get pending(): readonly Waiting[] {
    return this.waiting;
  }

  /**
   * Speak `text` for `agent`. Replaces a sentence for the same agent that has
   * not started yet; otherwise queues it.
   */
  say(agent: string, text: string): void {
    const trimmed = text.trim();
    if (trimmed === "") return;
    const at = this.waiting.findIndex((entry) => entry.agent === agent);
    if (at >= 0) {
      this.waiting[at] = { agent, text: trimmed };
    } else {
      this.waiting.push({ agent, text: trimmed });
    }
    void this.pump();
  }

  /** Stop the current sentence now and drop every waiting one. */
  interrupt(): void {
    this.waiting = [];
    this.current?.abort();
  }

  /** Be told when {@link speaking} changes. Returns the unsubscribe. */
  subscribe(listener: (speaking: boolean) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private setActive(active: boolean): void {
    if (this.active === active) return;
    this.active = active;
    for (const listener of this.listeners) listener(active);
  }

  private async pump(): Promise<void> {
    if (this.active) return;
    this.setActive(true);
    try {
      for (let next = this.waiting.shift(); next !== undefined; next = this.waiting.shift()) {
        const controller = new AbortController();
        this.current = controller;
        try {
          // Raced against the abort so an interrupt ends the sentence here at
          // once, whatever the voice does about its own signal.
          await Promise.race([this.speakOne(next.text, controller.signal), aborted(controller.signal)]);
        } catch (error) {
          if (!controller.signal.aborted) this.deps.onProblem?.(reason(error));
        } finally {
          this.current = null;
        }
      }
    } finally {
      this.setActive(false);
    }
  }

  private async speakOne(text: string, signal: AbortSignal): Promise<void> {
    const plan = await this.deps.plan();
    if (signal.aborted) return;
    switch (plan.kind) {
      case "system":
        return this.deps.system.speak(text, signal);
      case "unavailable":
        this.deps.onProblem?.(plan.reason);
        return;
      case "provider":
        try {
          await this.deps.provider.speak(text, signal);
        } catch (error) {
          if (signal.aborted || !plan.fallbackToSystem) throw error;
          await this.deps.system.speak(text, signal);
        }
    }
  }
}

function aborted(signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) resolve();
    else signal.addEventListener("abort", () => resolve(), { once: true });
  });
}

function reason(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : "speech failed";
}

/**
 * How long the system voice is given for `text` before it is cancelled: a
 * generous speaking rate plus a margin, so a voice that never reports the end
 * of an utterance cannot hold the queue.
 */
export function systemVoiceDeadlineMs(text: string): number {
  return Math.min(60_000, 3_000 + text.length * 120);
}

/** The parts of `speechSynthesis` the system voice uses. */
export interface SynthLike {
  speak(utterance: SpeechSynthesisUtterance): void;
  cancel(): void;
}

/** The operating system's voice, through the Web Speech API. */
export function systemVoice(
  synth: SynthLike | undefined = globalThis.speechSynthesis,
  utter: ((text: string) => SpeechSynthesisUtterance) | undefined = typeof SpeechSynthesisUtterance === "undefined"
    ? undefined
    : (text) => new SpeechSynthesisUtterance(text),
): SpeechVoice {
  return {
    speak(text, signal) {
      if (synth === undefined || utter === undefined) return Promise.reject(new Error(NO_SYSTEM_VOICE));
      if (signal.aborted) return Promise.resolve();
      return new Promise<void>((resolve, reject) => {
        const utterance = utter(text);
        let settled = false;
        const finish = (error?: Error) => {
          if (settled) return;
          settled = true;
          clearTimeout(deadline);
          signal.removeEventListener("abort", stop);
          if (error) reject(error);
          else resolve();
        };
        const stop = () => {
          synth.cancel();
          finish();
        };
        const deadline = setTimeout(stop, systemVoiceDeadlineMs(text));
        signal.addEventListener("abort", stop, { once: true });
        utterance.onend = () => finish();
        utterance.onerror = (event) => {
          // A cancel reports itself as an error; it is a stop, not a failure.
          if (signal.aborted || event.error === "interrupted" || event.error === "canceled") finish();
          else finish(new Error(`the system voice failed (${event.error})`));
        };
        synth.speak(utterance);
      });
    },
  };
}

/** The parts of an `AudioContext` the provider voice uses. */
export interface AudioContextLike {
  readonly destination: AudioNode;
  resume?(): Promise<void>;
  decodeAudioData(data: ArrayBuffer): Promise<AudioBuffer>;
  createBufferSource(): AudioBufferSourceNode;
}

/**
 * The Commands connection's text-to-speech: audio fetched Rust-side, decoded
 * and played with Web Audio. The context is created on first use and reused.
 */
export function providerVoice(
  fetchAudio: (text: string) => Promise<ArrayBuffer>,
  makeContext: () => AudioContextLike = () => new AudioContext(),
): SpeechVoice {
  let context: AudioContextLike | undefined;
  return {
    async speak(text, signal) {
      const audio = await fetchAudio(text);
      if (signal.aborted) return;
      context ??= makeContext();
      const ctx = context;
      await ctx.resume?.();
      const buffer = await ctx.decodeAudioData(audio);
      if (signal.aborted) return;
      const source = ctx.createBufferSource();
      source.buffer = buffer;
      source.connect(ctx.destination);
      await new Promise<void>((resolve) => {
        const stop = () => {
          try {
            source.stop();
          } catch {
            // Already stopped.
          }
          resolve();
        };
        signal.addEventListener("abort", stop, { once: true });
        source.onended = () => {
          signal.removeEventListener("abort", stop);
          resolve();
        };
        source.start();
      });
    },
  };
}

/** The queue reading mode uses, wired to the bridge and the real voices. */
export function createSpeechQueue(bridge: DeckBridge, onProblem?: (reason: string) => void): SpeechQueue {
  return new SpeechQueue({
    plan: () => bridge.voiceSpeechPlan(),
    provider: providerVoice((text) => bridge.voiceSpeechAudio(text)),
    system: systemVoice(),
    onProblem,
  });
}
