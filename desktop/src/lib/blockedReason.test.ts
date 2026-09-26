import { describe, expect, it } from "vitest";
import { blockedReasonText } from "./blockedReason";

describe("blockedReasonText", () => {
  it("prints the fixed kind label, then the scrubbed pane line", () => {
    expect(blockedReasonText({ kind: "credits_depleted", detectedAtMs: 1, detail: "purchase more credits" }))
      .toBe("Credits depleted (no reset) — purchase more credits");
    expect(blockedReasonText({ kind: "usage_limit", detectedAtMs: 1 })).toBe("Usage limit reached");
    expect(blockedReasonText(undefined)).toBe("Provider limit reached");
  });

  it("strips bidi and control characters from the agent-controlled detail", () => {
    const text = blockedReasonText({ kind: "usage_limit", detectedAtMs: 1, detail: "try‮ again\u0007" });
    expect(text).not.toContain("‮");
    expect(text).not.toContain("\u0007");
  });
});
