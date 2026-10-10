import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { FALLBACK_RECHECK_SECS, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";
import { useSelfUpgradeCheck } from "./useSelfUpgradeCheck";

const CHECK: SelfUpgradeCheck = {
  latest: "0.47.0",
  updateAvailable: true,
  notice: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)",
  app: { copy: "app", label: "Agent Deck (desktop app)", headline: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)", current: "0.46.0", latest: "0.47.0", action: "swap-app", actionable: true, confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.47.0?", provenance: { checked: true, reason: null }, lines: [] },
  cli: null,
  recheckAfterSecs: 3600,
};

function apiWith(check: SelfUpgradeApi["check"]): SelfUpgradeApi {
  return { check: vi.fn(check), run: vi.fn(), relaunch: vi.fn() };
}

/** Let the pending check's promise settle. */
const settle = () => act(async () => { await Promise.resolve(); });

describe("useSelfUpgradeCheck", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  /** Scenario: The app starts: it asks once straight away, then again only after the interval the answer names. */
  it("self_upgrade_check_001 checks at start and again after the recheck interval", async () => {
    const api = apiWith(async () => CHECK);
    const { result } = renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    expect(api.check).toHaveBeenCalledTimes(1);
    expect(result.current.check).toEqual(CHECK);

    await act(async () => { vi.advanceTimersByTime(CHECK.recheckAfterSecs * 1000 - 1); });
    expect(api.check).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(1); });
    expect(api.check).toHaveBeenCalledTimes(2);
  });

  /** Scenario: GitHub cannot be reached: no notice appears, and the check is tried again after the fallback interval. */
  it("self_upgrade_check_002 a failed check shows nothing and retries later", async () => {
    const api = apiWith(async () => { throw new Error("Cannot check for a newer release: offline"); });
    const { result } = renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    expect(result.current.check).toBeUndefined();
    await act(async () => { vi.advanceTimersByTime(FALLBACK_RECHECK_SECS * 1000); });
    expect(api.check).toHaveBeenCalledTimes(2);
  });

  /** Scenario: Outside the app (no Tauri bridge) nothing is ever checked. */
  it("self_upgrade_check_003 checks nothing without the app's bridge", async () => {
    const { result } = renderHook(() => useSelfUpgradeCheck(undefined));
    await settle();
    expect(result.current.check).toBeUndefined();
  });

  /** Scenario: Unmounting stops the periodic check. */
  it("self_upgrade_check_004 stops checking once unmounted", async () => {
    const api = apiWith(async () => CHECK);
    const { unmount } = renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    unmount();
    await act(async () => { vi.advanceTimersByTime(CHECK.recheckAfterSecs * 1000 * 3); });
    expect(api.check).toHaveBeenCalledTimes(1);
  });
});
