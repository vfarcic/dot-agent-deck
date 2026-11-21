import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { FALLBACK_RECHECK_SECS, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";
import { FOCUS_RECHECK_FLOOR_SECS, useSelfUpgradeCheck } from "./useSelfUpgradeCheck";

const CHECK: SelfUpgradeCheck = {
  checkId: 1,
  latest: "0.47.0",
  updateAvailable: true,
  notice: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)",
  installed: null,
  app: { copy: "app", label: "Agent Deck (desktop app)", headline: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)", current: "0.46.0", latest: "0.47.0", action: "swap-app", actionable: true, confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.47.0?", provenance: { checked: true, reason: null }, lines: [] },
  cli: null,
  recheckAfterSecs: 3600,
};

function apiWith(check: SelfUpgradeApi["check"]): SelfUpgradeApi {
  return { check: vi.fn(check), run: vi.fn(), relaunch: vi.fn() };
}

/** Let the pending check's promise settle. */
const settle = () => act(async () => { await Promise.resolve(); });

/** The machine slept for `ms`: the wall clock moved, and no timer advanced. */
const sleepFor = (ms: number) => vi.setSystemTime(Date.now() + ms);

/** The window came back: it gained focus and its page became visible again. */
const comeBack = () => act(async () => {
  window.dispatchEvent(new Event("focus"));
  document.dispatchEvent(new Event("visibilitychange"));
  await Promise.resolve();
});

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

  /** Scenario: Two checks are in flight — the periodic one and the one a closed dialog asked for — and the later one's answer arrives first: the earlier one's answer, arriving after it, does not replace it. */
  it("self_upgrade_check_005 an older answer arriving later never replaces a newer one", async () => {
    let resolveFirst: (answer: SelfUpgradeCheck) => void = () => undefined;
    const answers = [
      new Promise<SelfUpgradeCheck>((resolve) => { resolveFirst = resolve; }),
      Promise.resolve({ ...CHECK, checkId: 2, latest: "0.48.0" }),
    ];
    const api = apiWith(() => answers.shift()!);
    const { result } = renderHook(() => useSelfUpgradeCheck(api));
    await act(async () => { result.current.recheck(); });
    await settle();
    expect(result.current.check?.checkId).toBe(2);
    await act(async () => { resolveFirst({ ...CHECK, checkId: 1 }); });
    expect(result.current.check?.checkId).toBe(2);
    expect(result.current.check?.latest).toBe("0.48.0");
  });

  /** Scenario: The laptop sleeps for five hours, so the six-hour timer has not moved, and the user brings the window back: the app checks again straight away, once, though focus and visibility both report it. */
  it("self_upgrade_check_006 the window coming back after a long sleep checks again", async () => {
    const api = apiWith(async () => ({ ...CHECK, recheckAfterSecs: 6 * 60 * 60 }));
    renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    expect(api.check).toHaveBeenCalledTimes(1);

    sleepFor(5 * 60 * 60 * 1000);
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(2);
  });

  /** Scenario: The user switches away and back within the hour after a check: nothing is asked, so switching windows does not query GitHub repeatedly; once the last check is an hour old, coming back checks. */
  it("self_upgrade_check_007 coming back within the hour of the last check asks nothing", async () => {
    const api = apiWith(async () => ({ ...CHECK, recheckAfterSecs: 6 * 60 * 60 }));
    renderHook(() => useSelfUpgradeCheck(api));
    await settle();

    sleepFor(FOCUS_RECHECK_FLOOR_SECS * 1000 - 1);
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(1);

    sleepFor(1);
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(2);
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(2);
  });

  /** Scenario: The page becomes hidden (the window minimised) after an hour: nothing is checked until it is shown again. */
  it("self_upgrade_check_008 a page becoming hidden does not check", async () => {
    const api = apiWith(async () => CHECK);
    renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    sleepFor(FOCUS_RECHECK_FLOOR_SECS * 1000 * 2);
    const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    try {
      await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
      expect(api.check).toHaveBeenCalledTimes(1);
    } finally {
      visibility.mockRestore();
    }
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(2);
  });

  /** Scenario: Unmounting stops the periodic check, and the window coming back no longer checks either. */
  it("self_upgrade_check_004 stops checking once unmounted", async () => {
    const api = apiWith(async () => CHECK);
    const { unmount } = renderHook(() => useSelfUpgradeCheck(api));
    await settle();
    unmount();
    await act(async () => { vi.advanceTimersByTime(CHECK.recheckAfterSecs * 1000 * 3); });
    sleepFor(FOCUS_RECHECK_FLOOR_SECS * 1000 * 2);
    await comeBack();
    expect(api.check).toHaveBeenCalledTimes(1);
  });
});
