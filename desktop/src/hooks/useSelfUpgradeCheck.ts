import { useCallback, useEffect, useRef, useState } from "react";

import { FALLBACK_RECHECK_SECS, newerCheck, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";

/**
 * Issue #2450 — how old the last check must be before the window coming back
 * (focus, or the page becoming visible) checks again: an hour, so switching
 * windows does not ask GitHub each time. The TUI uses the same floor on a
 * terminal focus-in (`FOCUS_RECHECK_FLOOR` in `src/upgrade_dialog.rs`).
 */
export const FOCUS_RECHECK_FLOOR_SECS = 60 * 60;

/**
 * Issue #1635 — ask whether a newer release exists when the app starts, and
 * again every `recheckAfterSecs` (the crate's `UPDATE_RECHECK_INTERVAL`) while
 * it runs.
 *
 * That interval is a `setTimeout`, which does not advance while the machine
 * sleeps, so a laptop that sleeps often could go many hours past a release
 * without a check (issue #2450). The window gaining focus or becoming visible
 * therefore checks too, when the last check started at least
 * `FOCUS_RECHECK_FLOOR_SECS` ago by the wall clock, which does keep running
 * through a sleep.
 *
 * A check that could not be made — no network, GitHub refusing — leaves the
 * last answer in place and is tried again after the fallback interval. An
 * answer older than the one held (two checks in flight, the later finishing
 * first) is dropped. Nothing is checked without an `api` (outside the app).
 */
export function useSelfUpgradeCheck(api: SelfUpgradeApi | undefined): { check?: SelfUpgradeCheck; recheck: () => void } {
  const [check, setCheck] = useState<SelfUpgradeCheck>();
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const live = useRef(true);
  const run = useRef<() => void>(() => undefined);
  /** When the last check started, by `Date.now()`; undefined before the first. */
  const lastStarted = useRef<number>(undefined);

  run.current = () => {
    if (!api) return;
    clearTimeout(timer.current);
    lastStarted.current = Date.now();
    const schedule = (secs: number) => {
      if (live.current) timer.current = setTimeout(() => run.current(), Math.max(60, secs) * 1000);
    };
    api.check().then(
      (answer) => {
        if (!live.current) return;
        setCheck((current) => newerCheck(current, answer));
        schedule(answer.recheckAfterSecs);
      },
      () => schedule(FALLBACK_RECHECK_SECS),
    );
  };

  useEffect(() => {
    live.current = true;
    run.current();
    const onBack = () => {
      if (document.visibilityState === "hidden") return;
      const last = lastStarted.current;
      const since = last === undefined ? Infinity : Date.now() - last;
      // A clock that moved backwards counts as due: how long it has been is unknown.
      if (since < 0 || since >= FOCUS_RECHECK_FLOOR_SECS * 1000) run.current();
    };
    window.addEventListener("focus", onBack);
    document.addEventListener("visibilitychange", onBack);
    return () => {
      live.current = false;
      clearTimeout(timer.current);
      window.removeEventListener("focus", onBack);
      document.removeEventListener("visibilitychange", onBack);
    };
  }, [api]);

  const recheck = useCallback(() => run.current(), []);
  return { check, recheck };
}
