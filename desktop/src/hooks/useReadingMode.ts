/**
 * PRD #1497 M5 — the voice surface's reading mode and speech queue, built once
 * per mount from the runtime's seams.
 *
 * Besides the two objects, this keeps the one fact D8 needs: whether the app
 * spoke at any point during the segment the microphone is recording now. The
 * recording overlaps the work on the previous utterance (PR #1451), so a
 * segment runs from one `voiceStop` to the next — {@link UseReadingMode.takeSegment}
 * is called at each — and the queue's `speaking` going true marks the open
 * segment as overlapped. "Is it speaking now" would be the wrong question: a
 * recording of the app's own voice ends after the voice does.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { ReadingMode, type ReadingTarget } from "../lib/reading";
import { SpeechQueue, providerVoice, systemVoice } from "../lib/speech";
import type { DeckRuntimeState } from "../types";

/** Said when this build's runtime has no reading seam at all (the bare preview harness). */
export const READING_NOT_IN_THIS_RUNTIME = "Reading is not available: this app cannot read an agent's turns here.";

export interface UseReadingMode {
  mode: ReadingMode;
  queue: SpeechQueue;
  /** The agent being read, for the indicator. */
  reading: ReadingTarget | undefined;
  /** End the current recording segment: answers whether the app spoke during it, and starts the next. */
  takeSegment: () => boolean;
  /** The microphone opened afresh: a new segment starts now. */
  startSegment: () => void;
}

export function useReadingMode(
  runtime: Pick<DeckRuntimeState, "voiceSpeechPlan" | "voiceSpeechAudio" | "voiceReadingStart" | "voiceReadingStop" | "onVoiceReadingConsentOff">,
  onChange?: (target: ReadingTarget | undefined) => void,
  onProblem?: (reason: string) => void,
): UseReadingMode {
  const runtimeRef = useRef(runtime);
  runtimeRef.current = runtime;
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const onProblemRef = useRef(onProblem);
  onProblemRef.current = onProblem;
  const [reading, setReading] = useState<ReadingTarget>();

  const { mode, queue } = useMemo(() => {
    const speech = new SpeechQueue({
      plan: () => runtimeRef.current.voiceSpeechPlan?.() ?? Promise.resolve({ kind: "system" as const }),
      provider: providerVoice((text) => runtimeRef.current.voiceSpeechAudio?.(text) ?? Promise.reject(new Error("this app has no speech service"))),
      system: systemVoice(),
      onProblem: (reason) => onProblemRef.current?.(reason),
    });
    const reader = new ReadingMode({
      start: (target, onSentence) => runtimeRef.current.voiceReadingStart?.(target, onSentence)
        ?? Promise.resolve({ kind: "unavailable" as const, sentence: READING_NOT_IN_THIS_RUNTIME }),
      stop: (session) => runtimeRef.current.voiceReadingStop?.(session) ?? Promise.resolve(),
      speech,
      onChange: (target) => {
        setReading(target);
        onChangeRef.current?.(target);
      },
    });
    return { mode: reader, queue: speech };
  }, []);

  const spoke = useRef(false);
  useEffect(() => queue.subscribe((speaking) => {
    if (speaking) spoke.current = true;
  }), [queue]);
  /* A save from any window that turned reading's opt-in off ends reading
     here too — a drain or a start in progress included, which the Rust side
     has no session for (PR #1617's fourth review). */
  useEffect(() => {
    let unsubscribe: (() => void) | undefined;
    let gone = false;
    void runtimeRef.current.onVoiceReadingConsentOff?.(() => { void mode.consentOff(); })
      .then((stop) => {
        if (gone) stop();
        else unsubscribe = stop;
      })
      .catch(() => undefined);
    return () => {
      gone = true;
      unsubscribe?.();
    };
  }, [mode]);
  // Unmounting ends the session and silences the app, without a sentence.
  useEffect(() => () => { void mode.dispose(); }, [mode]);

  return {
    mode,
    queue,
    reading,
    takeSegment: () => {
      const overlapped = spoke.current || queue.speaking;
      spoke.current = queue.speaking;
      return overlapped;
    },
    startSegment: () => {
      spoke.current = queue.speaking;
    },
  };
}
