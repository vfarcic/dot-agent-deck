import { describe, expect, it } from "vitest";
import {
  WINDOWS_ORCHESTRATION_BLOCK_REASON,
  desktopOrchestrationPlatformIssue,
  isWindowsPlatform,
} from "./platform";

describe("desktop orchestration platform guard", () => {
  /** Scenario: Windows platform hints are detected across WebView variants. */
  it("detects Windows from either WebView platform hint", () => {
    expect(isWindowsPlatform({ platform: "Win32", userAgent: "neutral" })).toBe(true);
    expect(isWindowsPlatform({ platform: "x86_64", userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" })).toBe(true);
    expect(isWindowsPlatform({ platform: "x86_64", userAgentData: { platform: "Windows" } })).toBe(true);
  });

  /** Scenario: macOS and Linux can activate orchestrations without a platform warning. */
  it("keeps macOS and Linux orchestration activation available", () => {
    expect(desktopOrchestrationPlatformIssue({ platform: "MacIntel", userAgent: "Mozilla/5.0 (Macintosh)" })).toBeUndefined();
    expect(desktopOrchestrationPlatformIssue({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" })).toBeUndefined();
  });

  /** Scenario: Windows shows the specific command quoting limitation for orchestration activation. */
  it("returns the explicit Windows limitation instead of guessing a command dialect", () => {
    expect(desktopOrchestrationPlatformIssue({ platform: "Win32" })).toBe(WINDOWS_ORCHESTRATION_BLOCK_REASON);
    expect(WINDOWS_ORCHESTRATION_BLOCK_REASON).toMatch(/POSIX shell quoting/);
  });
});
