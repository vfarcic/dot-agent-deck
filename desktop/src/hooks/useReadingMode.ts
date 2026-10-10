/**
 * PRD #1497 — the voice surface's reader and speech queue, built once per
 * mount from the runtime's seams.
 *
 * Besides the two objects, this keeps the one fact D8 needs: whether the app
 * spoke at any point during the segment the microphone is recording now. The
 * recording overlaps the work on the previous utterance (PR #1451), so a
 * segment runs from one `voiceStop` to the next — {@link UseReadingMode.takeSegment}
 * is called at each — and the queue's `speaking` going true marks the open
 * segment as overlapped. "Is it speaking now" would be the wrong question: a
 * recording of the app's own voice ends after the voice does.
 */

import { useEffect, useMemo, useRef } from "react";
import { DeckReader } from "../lib/reading";
import { SpeechQueue, providerVoice, systemVoice } from "../lib/speech";
import type { DeckRuntimeState } from "../types";

/** Said when this build's runtime has no reading seam at all (the bare preview harness). */
export const READING_NOT_IN_THIS_RUNTIME = "Reading is not available: this app cannot read an agent's turns here.";

export interface UseReadingMode {
  reader: DeckReader;
  queue: SpeechQueue;
  /** End the current recording segment: answers whether the app spoke during it, and starts the next. */
  takeSegment: () => boolean;
  /** The microphone opened afresh: a new segment starts now. */
  startSegment: () => void;
}

export function useReadingMode(
  runtime: Pick<DeckRuntimeState, "voiceSpeechPlan" | "voiceSpeechAudio" | "voiceReadingStart" | "voiceReadingStop" | "onVoiceReadingConsentOff" | "onVoiceReadingConsentOn">,
  openPane: () => { deckId: string; agentId: string } | undefined,
  onProblem?: (reason: string) => void,
  onReadingProblem?: (sentence: string) => void,
): UseReadingMode {
  const runtimeRef = useRef(runtime);
  runtimeRef.current = runtime;
  const openPaneRef = useRef(openPane);
  openPaneRef.current = openPane;
  const onProblemRef = useRef(onProblem);
  onProblemRef.current = onProblem;
  const onReadingProblemRef = useRef(onReadingProblem);
  onReadingProblemRef.current = onReadingProblem;
  /* Settles once this window can hear a consent-off save (below); reading
     starts only after it, and not at all if it rejects. */
  const listening = useRef<Promise<void> | undefined>(undefined);
  /* Settles once this window can hear a consent-on save, or could not
     install that listener and has told the reader so. Reading starts only
     after it too, so a save reported on after a start read the settings is
     never missed (audit A1). */
  const hearingOn = useRef<Promise<void> | undefined>(undefined);

  const { reader, queue } = useMemo(() => {
    const speech = new SpeechQueue({
      plan: () => runtimeRef.current.voiceSpeechPlan?.() ?? Promise.resolve({ kind: "system" as const }),
      provider: providerVoice((text) => runtimeRef.current.voiceSpeechAudio?.(text) ?? Promise.reject(new Error("this app has no speech service"))),
      system: systemVoice(),
      onProblem: (reason) => onProblemRef.current?.(reason),
    });
    const deckReader = new DeckReader({
      start: async (target, onSentence) => {
        const start = runtimeRef.current.voiceReadingStart;
        if (start === undefined) return { kind: "unavailable" as const, sentence: READING_NOT_IN_THIS_RUNTIME, scope: "deck" as const };
        /* A window that cannot hear the switch go off never reads, and that is
           about every agent, so it is said once. A rejection of the start
           itself is that one agent's failure, said as such. */
        try {
          await listening.current;
        } catch {
          return { kind: "unavailable" as const, sentence: READING_NOT_IN_THIS_RUNTIME, scope: "deck" as const };
        }
        await hearingOn.current;
        return start(target, onSentence);
      },
      stop: (session) => runtimeRef.current.voiceReadingStop?.(session) ?? Promise.resolve(),
      speech,
      openPane: () => openPaneRef.current(),
      onProblem: (sentence) => onReadingProblemRef.current?.(sentence),
    });
    return { reader: deckReader, queue: speech };
  }, []);

  const spoke = useRef(false);
  useEffect(() => queue.subscribe((speaking) => {
    if (speaking) spoke.current = true;
  }), [queue]);
  /* A save from any window that turned reading's switch off ends reading
     here too — a start in progress included, which the Rust side has no
     session for (PR #1617's fourth review). Reading waits for this listener,
     and a window that could not install it never reads: it would not hear
     the switch go off (PR #1617's fifth review). A save that left it on
     retries a start that read the document before that save reached it. */
  useEffect(() => {
    const unsubscribe: Array<() => void> = [];
    let gone = false;
    const keep = (stop: () => void) => {
      if (gone) stop();
      else unsubscribe.push(stop);
    };
    const subscribeOff = runtimeRef.current.onVoiceReadingConsentOff;
    const installed = (subscribeOff?.(() => { reader.consentOff(); }) ?? Promise.reject(new Error("no consent-off listener"))).then(keep);
    listening.current = installed;
    installed.catch(() => undefined);
    /* A window that cannot hear a save report the switch on still reads: the
       reader asks a refused agent again on its own schedule instead of
       waiting for that report (PR #1617's Qodo review). It is told before
       any start runs, since starts wait for this. Asked inside a promise, so
       a seam that throws counts as one that rejected. */
    const subscribeOn = runtimeRef.current.onVoiceReadingConsentOn;
    if (subscribeOn !== undefined) {
      hearingOn.current = Promise.resolve().then(() => subscribeOn(() => { reader.consentOn(); })).then(keep, () => { reader.consentOnUnheard(); });
    } else {
      reader.consentOnUnheard();
    }
    return () => {
      gone = true;
      for (const stop of unsubscribe) stop();
    };
  }, [reader]);
  // Unmounting stops every session and silences the app, without a sentence.
  useEffect(() => () => { reader.dispose(); }, [reader]);

  return {
    reader,
    queue,
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
