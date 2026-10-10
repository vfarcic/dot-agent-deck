import { useCallback, useEffect, useRef, useState } from "react";

import { FALLBACK_RECHECK_SECS, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";

/**
 * Issue #1635 — ask whether a newer release exists when the app starts, and
 * again every `recheckAfterSecs` (the crate's `UPDATE_RECHECK_INTERVAL`) while
 * it runs.
 *
 * A check that could not be made — no network, GitHub refusing — leaves the
 * last answer in place and is tried again after the fallback interval. Nothing
 * is checked without an `api` (outside the app).
 */
export function useSelfUpgradeCheck(api: SelfUpgradeApi | undefined): { check?: SelfUpgradeCheck; recheck: () => void } {
  const [check, setCheck] = useState<SelfUpgradeCheck>();
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const live = useRef(true);
  const run = useRef<() => void>(() => undefined);

  run.current = () => {
    if (!api) return;
    clearTimeout(timer.current);
    const schedule = (secs: number) => {
      if (live.current) timer.current = setTimeout(() => run.current(), Math.max(60, secs) * 1000);
    };
    api.check().then(
      (answer) => {
        if (!live.current) return;
        setCheck(answer);
        schedule(answer.recheckAfterSecs);
      },
      () => schedule(FALLBACK_RECHECK_SECS),
    );
  };

  useEffect(() => {
    live.current = true;
    run.current();
    return () => {
      live.current = false;
      clearTimeout(timer.current);
    };
  }, [api]);

  const recheck = useCallback(() => run.current(), []);
  return { check, recheck };
}
