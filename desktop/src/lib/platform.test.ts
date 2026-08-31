import { describe, expect, it } from "vitest";
import {
  WINDOWS_WORKFLOW_BLOCK_REASON,
  desktopWorkflowPlatformIssue,
  isWindowsPlatform,
} from "./platform";

describe("desktop workflow platform guard", () => {
  it("detects Windows from either WebView platform hint", () => {
    expect(isWindowsPlatform({ platform: "Win32", userAgent: "neutral" })).toBe(true);
    expect(isWindowsPlatform({ platform: "x86_64", userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" })).toBe(true);
    expect(isWindowsPlatform({ platform: "x86_64", userAgentData: { platform: "Windows" } })).toBe(true);
  });

  it("keeps macOS and Linux workflow launch available", () => {
    expect(desktopWorkflowPlatformIssue({ platform: "MacIntel", userAgent: "Mozilla/5.0 (Macintosh)" })).toBeUndefined();
    expect(desktopWorkflowPlatformIssue({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" })).toBeUndefined();
  });

  it("returns the explicit Windows limitation instead of guessing a command dialect", () => {
    expect(desktopWorkflowPlatformIssue({ platform: "Win32" })).toBe(WINDOWS_WORKFLOW_BLOCK_REASON);
    expect(WINDOWS_WORKFLOW_BLOCK_REASON).toMatch(/POSIX shell quoting/);
  });
});
