export const WINDOWS_WORKFLOW_BLOCK_REASON = "Live workflow launch is unavailable in this Windows preview because profile commands use POSIX shell quoting. Use the TUI or launch commands manually until native Windows command construction is implemented.";

export interface BrowserPlatformHints {
  platform?: string;
  userAgent?: string;
  userAgentData?: { platform?: string };
}

export function isWindowsPlatform(hints?: BrowserPlatformHints): boolean {
  if (!hints) return false;
  return [hints.userAgentData?.platform, hints.platform, hints.userAgent]
    .filter((value): value is string => typeof value === "string")
    .some((value) => /windows|^win(?:32|64|ce)/i.test(value));
}

export function desktopWorkflowPlatformIssue(
  hints: BrowserPlatformHints | undefined = typeof navigator === "undefined" ? undefined : navigator,
): string | undefined {
  return isWindowsPlatform(hints) ? WINDOWS_WORKFLOW_BLOCK_REASON : undefined;
}
