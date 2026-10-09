import { createContext, useContext, useLayoutEffect, useState, type RefObject } from "react";
import type { NumberedLayer } from "./useVoiceNumbers";
import type { VoicePager } from "../lib/voicePages";

/**
 * PR #1451 round 3, change 4 — where a list split into pages tells the shell
 * how to turn it, in the same two layers a numbered list declares in
 * (`useVoiceNumbers`): `dialog` while the New agent dialog or the Daemon
 * selector is open, `screen` for the Daemons screen. "next page" turns the
 * dialog's list while there is one and the screen's otherwise; a layer that
 * pages nothing publishes `undefined`. The agent dashboard publishes nothing:
 * it scrolls rather than paging (issue #1492), and the shell scrolls it on a
 * page turn when no layer pages.
 */
export interface VoicePaging {
  publish: (layer: NumberedLayer, pager: VoicePager | undefined) => void;
}

export const VoicePagingContext = createContext<VoicePaging | undefined>(undefined);

/** Publish `pager` as `layer`'s for as long as the caller is mounted, withdrawn on unmount. */
export function usePager(layer: NumberedLayer, pager: VoicePager | undefined): void {
  const paging = useContext(VoicePagingContext);
  useLayoutEffect(() => {
    paging?.publish(layer, pager);
  }, [layer, pager, paging]);
  useLayoutEffect(() => () => paging?.publish(layer, undefined), [layer, paging]);
}

/** An element's content box, in CSS pixels. */
export interface MeasuredBox {
  width: number;
  height: number;
}

/**
 * The content box of `ref`'s element while `enabled`, re-measured whenever it
 * or the window resizes — what a paged list fills. `undefined` while disabled,
 * before the first measurement, and in a runtime that lays nothing out (jsdom
 * reports every box as 0 × 0), where the caller pages by a fixed size instead:
 * that fallback is the deterministic seam the component tests drive.
 */
export function useMeasuredBox(ref: RefObject<HTMLElement | null>, enabled: boolean): MeasuredBox | undefined {
  const [box, setBox] = useState<MeasuredBox>();
  useLayoutEffect(() => {
    const element = ref.current;
    if (!enabled || !element) {
      setBox(undefined);
      return;
    }
    const measure = () => {
      const style = getComputedStyle(element);
      const px = (value: string) => Number.parseFloat(value) || 0;
      const width = element.clientWidth - px(style.paddingLeft) - px(style.paddingRight);
      const height = element.clientHeight - px(style.paddingTop) - px(style.paddingBottom);
      const next = width > 0 && height > 0 ? { width, height } : undefined;
      setBox((current) => (current?.width === next?.width && current?.height === next?.height ? current : next));
    };
    measure();
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(measure);
    observer?.observe(element);
    window.addEventListener("resize", measure);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [enabled, ref]);
  return enabled ? box : undefined;
}
