import { describe, expect, it } from "vitest";
import { formatElapsedSeconds, questionAgeSeconds, questionBadgeText } from "./questionAge";

describe("a waiting question's age (issue #2075)", () => {
  /**
   * Scenario (question/desktop/035): the tile writes a question's age exactly
   * as the TUI card writes its elapsed times — the examples are
   * `test_format_elapsed`'s in `src/ui.rs` — and the badge reads `QUESTION`
   * followed by that age, counted from when the agent raised the question.
   */
  it("question/desktop/035: the age reads as the TUI card writes it", () => {
    expect(formatElapsedSeconds(0)).toBe("0s");
    expect(formatElapsedSeconds(3)).toBe("3s");
    expect(formatElapsedSeconds(90)).toBe("1m 30s");
    expect(formatElapsedSeconds(60)).toBe("1m");
    expect(formatElapsedSeconds(3900)).toBe("1h 5m");
    expect(formatElapsedSeconds(3600)).toBe("1h");
    // No day unit: hours keep counting, as in the Rust function.
    expect(formatElapsedSeconds(26 * 3600 + 3 * 60)).toBe("26h 3m");
    // A start ahead of now (clock skew) reads zero, not a negative age.
    expect(formatElapsedSeconds(-30)).toBe("0s");
    expect(questionAgeSeconds(1_000_000 + 30_000, 1_000_000)).toBe(0);

    const raisedAtMs = 1_767_225_600_000;
    expect(questionBadgeText({ raisedAtMs }, raisedAtMs + 12 * 60_000)).toBe("QUESTION 12m");
    expect(questionBadgeText({ raisedAtMs }, raisedAtMs + 3 * 3_600_000 + 30 * 60_000)).toBe("QUESTION 3h 30m");
    // A daemon that reported no time gives no age to count.
    expect(questionBadgeText({}, raisedAtMs)).toBe("QUESTION");
    expect(questionBadgeText({ raisedAtMs: 0 }, raisedAtMs)).toBe("QUESTION");
  });
});
