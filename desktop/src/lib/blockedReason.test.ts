import { describe, expect, it } from "vitest";
import { blockedReasonText, formatSpan } from "./blockedReason";

describe("blockedReasonText", () => {
  it("prints the fixed kind label, then the scrubbed agent message", () => {
    expect(blockedReasonText({ kind: "credits_depleted", detectedAtMs: 1, detail: "purchase more credits" }))
      .toBe("Credits depleted (no reset) — purchase more credits");
    expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: 1 })).toBe("Usage limit reached");
    expect(blockedReasonText(undefined)).toBe("Provider limit reached");
  });

  it("says when the provider's limit resets, while that is still ahead", () => {
    const now = 1_000_000_000_000;
    const resetsAtMs = now + (2 * 60 + 10) * 60_000;
    expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: now, resetsAtMs }, now))
      .toBe("Usage limit reached · resets in 2h 10m");
    expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: now, resetsAtMs, detail: "limit hit" }, now))
      .toBe("Usage limit reached · resets in 2h 10m — limit hit");
    expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: now, resetsAtMs: now - 1 }, now))
      .toBe("Usage limit reached");
    // A reset no provider could send — past the crate's plausible window
    // (`QUOTA_RESET_MAX_FUTURE_MS`) — prints no countdown, as on the TUI card.
    for (const resetsAtMs of [Number.MAX_SAFE_INTEGER, now + 401 * 86_400_000]) {
      expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: now, resetsAtMs }, now))
        .toBe("Usage limit reached");
    }
    expect(formatSpan(59_000)).toBe("59 seconds");
    expect(formatSpan(60_000)).toBe("1 minute");
    expect(formatSpan(3 * 3_600_000)).toBe("3 hours");
  });

  it("strips bidi and control characters from the agent-controlled detail", () => {
    const text = blockedReasonText({ kind: "usage_limit", detectedAtMs: 1, detail: "try\u202e again\u0007" });
    expect(text).not.toContain("\u202e");
    expect(text).not.toContain("\u0007");
  });
});
