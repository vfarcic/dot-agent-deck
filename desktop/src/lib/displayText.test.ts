import { describe, expect, it } from "vitest";
import { clampText, CLOCK_SKEW_TOLERANCE_MS, DISPLAY_LIMITS, displayActivity, displayIdentity, displayPath, displayText, displayUptime, domIdentity, homeRelative, rendersBlank, sanitizeText } from "./displayText";

/**
 * Every bidi formatting and override codepoint the Rust policy names
 * (`src/untrusted_text.rs::is_bidi_format_char`), enumerated rather than
 * sampled. Its own test does exactly this, for exactly this reason: a range
 * typo leaves one override live, and one is all a spoof needs.
 */
const BIDI_CODEPOINTS = [
  "\u202A", "\u202B", "\u202C", "\u202D", "\u202E",
  "\u2066", "\u2067", "\u2068", "\u2069",
  "\u200E", "\u200F", "\u061C",
];

describe("sanitizeText", () => {
  it("drops every bidi formatting codepoint the Rust policy names", () => {
    for (const codepoint of BIDI_CODEPOINTS) {
      expect(sanitizeText(`a${codepoint}b`)).toBe("ab");
    }
  });

  it("drops control characters — ANSI escapes, NUL, newline, DEL and the C1 range", () => {
    expect(sanitizeText("ze\u001b[31mta\0-li\u202eve\u007f-\u008577")).toBe("ze[31mta-live-77");
    expect(sanitizeText("one\ntwo\rthree")).toBe("onetwothree");
    for (const code of [0x00, 0x07, 0x1b, 0x1f, 0x7f, 0x80, 0x9f]) {
      expect(sanitizeText(`a${String.fromCharCode(code)}b`)).toBe("ab");
    }
  });

  it("keeps zero-width joiners and the other default-ignorable characters", () => {
    // A recorded decision, not an oversight: ZWJ/ZWNJ cannot reorder text, so
    // they do not produce the spoof this filter exists to stop, and they are
    // load-bearing in emoji sequences and in Persian, Arabic and Indic
    // orthography. Widening past the Rust policy would also make the TUI and
    // the desktop disagree about the same daemon string.
    expect(sanitizeText("\u{1f468}\u200d\u{1f4bb}")).toBe("\u{1f468}\u200d\u{1f4bb}");
    expect(sanitizeText("mi\u200cana")).toBe("mi\u200cana");
  });

  it("leaves ordinary text alone, accents, CJK and emoji included", () => {
    expect(sanitizeText("résumé · 日本語 · 🚀 · a/b-c_d")).toBe("résumé · 日本語 · 🚀 · a/b-c_d");
  });
});

describe("clampText", () => {
  it("passes anything within the budget through untouched", () => {
    expect(clampText("short", 10)).toBe("short");
    expect(clampText("exactly-10", 10)).toBe("exactly-10");
  });

  it("marks a clamped value so it cannot pass itself off as complete", () => {
    expect(clampText("abcdefghij", 4)).toBe("abcd…");
  });

  it("counts code points, so a surrogate pair is never split in half", () => {
    const clamped = clampText("🚀🚀🚀🚀", 2);
    expect(clamped).toBe("🚀🚀…");
    expect(Array.from(clamped)).toHaveLength(3);
  });
});

describe("homeRelative", () => {
  it("replaces a leading home directory on every platform shape", () => {
    expect(homeRelative("/home/dev/code/deck")).toBe("~/code/deck");
    expect(homeRelative("/home/dev")).toBe("~");
    expect(homeRelative("/Users/dev/code/deck")).toBe("~/code/deck");
    expect(homeRelative("/root/code/deck")).toBe("~/code/deck");
    expect(homeRelative("C:\\Users\\dev\\code\\deck")).toBe("~\\code\\deck");
  });

  it("leaves anything that is not under a home directory alone", () => {
    expect(homeRelative("/srv/work/deck")).toBe("/srv/work/deck");
    expect(homeRelative("/homework/deck")).toBe("/homework/deck");
    expect(homeRelative("/home")).toBe("/home");
  });
});

describe("displayPath and displayText", () => {
  it("sanitises before abbreviating, so a planted escape cannot hide the prefix", () => {
    expect(displayPath("/ho\u202eme/dev/code/deck")).toBe("~/code/deck");
  });

  it("bounds what it returns however long the input is", () => {
    expect(Array.from(displayText("x".repeat(500), DISPLAY_LIMITS.toolDetail))).toHaveLength(DISPLAY_LIMITS.toolDetail + 1);
    expect(Array.from(displayPath(`/home/dev/${"x".repeat(500)}`))).toHaveLength(DISPLAY_LIMITS.path + 1);
  });
});

/**
 * Characters that survive the sanitiser and render as nothing. Retaining them
 * is a recorded decision — they are load-bearing in emoji sequences and in
 * several scripts — so the blank-identity spoof they enable is closed by a
 * visible fallback rather than by stripping one more codepoint.
 */
const INVISIBLE_CODEPOINTS = ["\u200b", "\u200c", "\u200d", "\u2060", "\ufeff", "\u00ad", "\ufe0f"];

/** A ZWJ emoji sequence and a Persian ZWNJ word: invisible characters doing real work. */
const JOINED_EMOJI = "\u{1f468}\u200d\u{1f4bb}";
const ZWNJ_WORD = "mi\u200cana";

describe("rendersBlank", () => {
  it("calls a value blank when nothing in it would be visible", () => {
    expect(rendersBlank("")).toBe(true);
    expect(rendersBlank("   ")).toBe(true);
    for (const codepoint of INVISIBLE_CODEPOINTS) {
      const name = `U+${codepoint.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0")}`;
      expect(rendersBlank(codepoint.repeat(4)), `${name} was not recognised as invisible`).toBe(true);
    }
    expect(rendersBlank(INVISIBLE_CODEPOINTS.join(""))).toBe(true);
  });

  it("calls anything with one visible character not blank, however much padding surrounds it", () => {
    expect(rendersBlank("a")).toBe(false);
    expect(rendersBlank("\u200b\u200b.\u200b")).toBe(false);
    expect(rendersBlank(JOINED_EMOJI)).toBe(false);
    expect(rendersBlank(ZWNJ_WORD)).toBe(false);
  });
});

describe("displayIdentity", () => {
  it("substitutes the fallback for a name that would render as an empty cell", () => {
    expect(displayIdentity("\u200b\u200c\u200d\ufeff", DISPLAY_LIMITS.name, "unnamed agent 7")).toBe("unnamed agent 7");
    expect(displayIdentity("", DISPLAY_LIMITS.name, "unnamed agent 7")).toBe("unnamed agent 7");
    // A name left invisible by the sanitiser rather than born that way counts too.
    expect(displayIdentity("\u202e", DISPLAY_LIMITS.name, "unnamed agent 7")).toBe("unnamed agent 7");
  });

  it("judges blankness before the clamp, so an elision marker cannot rescue an invisible name", () => {
    // `clampText` appends a visible `…`, so a long invisible name would test as
    // visible if blankness were judged on the clamped copy.
    expect(displayIdentity("\u200b".repeat(600), DISPLAY_LIMITS.name, "unnamed agent 7")).toBe("unnamed agent 7");
  });

  it("leaves a name that renders anything at all alone, ZWJ sequences included", () => {
    expect(displayIdentity("coder", DISPLAY_LIMITS.name, "unnamed agent 7")).toBe("coder");
    expect(displayIdentity(`team ${JOINED_EMOJI}`, DISPLAY_LIMITS.name, "x")).toBe(`team ${JOINED_EMOJI}`);
    expect(displayIdentity(ZWNJ_WORD, DISPLAY_LIMITS.name, "x")).toBe(ZWNJ_WORD);
  });
});

describe("domIdentity", () => {
  it("passes anything a healthy daemon reports through byte for byte", () => {
    expect(domIdentity("orchestration:orc-745")).toBe("orchestration:orc-745");
    expect(domIdentity("%2Ftmp%2Fdot-agent-deck.sock:12")).toBe("%2Ftmp%2Fdot-agent-deck.sock:12");
  });

  it("bounds a value the daemon left unbounded", () => {
    const bounded = domIdentity(`orchestration:${"x".repeat(50_000)}`);
    expect(Array.from(bounded).length).toBeLessThanOrEqual(DISPLAY_LIMITS.domIdentity + 8);
    expect(bounded.startsWith("orchestration:")).toBe(true);
  });

  it("keeps two over-long identities that share a prefix apart", () => {
    const prefix = "x".repeat(500);
    expect(domIdentity(`${prefix}a`)).not.toBe(domIdentity(`${prefix}b`));
  });

  it("strips control and bidi characters, which have no business in a DOM attribute", () => {
    expect(domIdentity("/tmp/de\u202eck.sock")).toBe("/tmp/deck.sock");
    expect(domIdentity("/tmp/de\nck.sock")).toBe("/tmp/deck.sock");
  });
});

describe("displayActivity", () => {
  /** A fixed "now", so every case below is arithmetic rather than a race. */
  const NOW = Date.parse("2026-09-01T12:00:00.000Z");
  const ago = (ms: number) => displayActivity(NOW - ms, NOW);

  it("names one unit, the largest that fits, floored", () => {
    expect(ago(0)?.label).toBe("just now");
    expect(ago(59_999)?.label).toBe("just now");
    expect(ago(60_000)?.label).toBe("1m ago");
    expect(ago(119_999)?.label).toBe("1m ago");
    expect(ago(59 * 60_000)?.label).toBe("59m ago");
    expect(ago(60 * 60_000)?.label).toBe("1h ago");
    expect(ago(23.9 * 60 * 60_000)?.label).toBe("23h ago");
    expect(ago(24 * 60 * 60_000)?.label).toBe("1d ago");
    expect(ago(46 * 60 * 60_000)?.label).toBe("1d ago");
    expect(ago(3650 * 24 * 60 * 60_000)?.label).toBe("3650d ago");
  });

  it("carries the exact UTC instant for the hover, so nothing is hidden behind the rounding", () => {
    expect(ago(90 * 60_000)).toEqual({ label: "1h ago", title: "2026-09-01T10:30:00.000Z" });
  });

  /**
   * The daemon reported no instant. Every agent under a RESTARTED daemon is
   * this case — it persists no session state, so `AgentRecord.live` is absent
   * and there is no activity time to report rather than a set of freshly minted
   * ones. Nothing to render, and nothing is what the column shows.
   */
  it("renders nothing when the daemon reported no instant", () => {
    expect(displayActivity(undefined, NOW)).toBeUndefined();
  });

  /**
   * `DesktopAgentDto` is a TypeScript assertion about a shape, not a validated
   * value, so a malformed daemon can put anything here. None of it may reach
   * the DOM as `NaN ago` or throw out of `toISOString`.
   */
  it("renders nothing for a value that is not a usable instant", () => {
    expect(displayActivity(Number.NaN, NOW)).toBeUndefined();
    expect(displayActivity(Number.POSITIVE_INFINITY, NOW)).toBeUndefined();
    expect(displayActivity(Number.NEGATIVE_INFINITY, NOW)).toBeUndefined();
    // Outside `Date`'s ±100,000,000-day range, where `toISOString()` throws.
    // An `i64` reaches nine orders of magnitude further than a `Date` does.
    expect(displayActivity(8.65e15, NOW)).toBeUndefined();
    expect(displayActivity(-8.65e15, NOW)).toBeUndefined();
    expect(displayActivity(Number.MAX_SAFE_INTEGER, NOW)).toBeUndefined();
  });

  /**
   * The clock-skew decision, both halves of it.
   *
   * The instant is stamped by whichever hook process emitted the event, not by
   * the daemon and not by the webview, so a small positive skew is ordinary and
   * reads "just now". A NEGATIVE "ago" is never rendered — but nor is a stamp
   * genuinely ahead of the webview's clock quietly rewritten into "just now":
   * `last_activity` is producer-supplied and deliberately unclamped daemon-side
   * (it is the ordering evidence `supersedes_generation` weighs, and the daemon
   * has a test for a report stamped ten years out), so calling a far-future
   * value "just now" would be exactly the fabricated reading PRD #745 refuses.
   * Beyond the tolerance the webview does not know, and says nothing.
   */
  it("absorbs ordinary clock skew and refuses to relativise anything beyond it", () => {
    expect(displayActivity(NOW + 1, NOW)?.label).toBe("just now");
    expect(displayActivity(NOW + CLOCK_SKEW_TOLERANCE_MS, NOW)?.label).toBe("just now");
    expect(displayActivity(NOW + CLOCK_SKEW_TOLERANCE_MS + 1, NOW)).toBeUndefined();
    // An hour, a day and ten years ahead are all the same answer: nothing.
    expect(displayActivity(NOW + 60 * 60_000, NOW)).toBeUndefined();
    expect(displayActivity(NOW + 3650 * 24 * 60 * 60_000, NOW)).toBeUndefined();
  });

  it("defaults `now` to the real clock, so callers need not pass one", () => {
    expect(displayActivity(Date.now())?.label).toBe("just now");
  });
});

describe("displayUptime", () => {
  /** A fixed "now", so every case below is arithmetic rather than a race. */
  const NOW = Date.parse("2026-09-01T12:00:00.000Z");
  const upFor = (ms: number) => displayUptime(NOW - ms, NOW);

  /**
   * The same buckets `displayActivity` uses, worded as a SPAN. No "ago",
   * because the interval named is still running — an uptime that read "3h ago"
   * would be a different and wrong claim about the same number.
   */
  it("names one unit, the largest that fits, floored, with no `ago`", () => {
    expect(upFor(0)?.label).toBe("<1m");
    expect(upFor(59_999)?.label).toBe("<1m");
    expect(upFor(60_000)?.label).toBe("1m");
    expect(upFor(119_999)?.label).toBe("1m");
    expect(upFor(59 * 60_000)?.label).toBe("59m");
    expect(upFor(60 * 60_000)?.label).toBe("1h");
    expect(upFor(23.9 * 60 * 60_000)?.label).toBe("23h");
    expect(upFor(24 * 60 * 60_000)?.label).toBe("1d");
    expect(upFor(46 * 60 * 60_000)?.label).toBe("1d");
    expect(upFor(3650 * 24 * 60 * 60_000)?.label).toBe("3650d");
  });

  it("carries the exact UTC spawn instant for the hover, so the rounding hides nothing", () => {
    expect(upFor(90 * 60_000)).toEqual({ label: "1h", title: "2026-09-01T10:30:00.000Z" });
  });

  /**
   * The daemon reported no spawn instant: it did not spawn this agent (an
   * id-only `ListAgents` reply from an older daemon) or it predates the field.
   * Nothing to render, and nothing is what the column shows — there is no
   * `Date.now()` fallback anywhere on this path, which is precisely the
   * fabrication the PRD's original duration rejection was about.
   */
  it("renders nothing when the daemon reported no spawn instant", () => {
    expect(displayUptime(undefined, NOW)).toBeUndefined();
  });

  /**
   * `DesktopAgentDto` is a TypeScript assertion about a shape, not a validated
   * value, so the same unusable values `displayActivity` refuses are refused
   * here — because both go through ONE shared guard rather than two copies of
   * one policy.
   */
  it("renders nothing for a value that is not a usable instant", () => {
    expect(displayUptime(Number.NaN, NOW)).toBeUndefined();
    expect(displayUptime(Number.POSITIVE_INFINITY, NOW)).toBeUndefined();
    expect(displayUptime(Number.NEGATIVE_INFINITY, NOW)).toBeUndefined();
    // Outside `Date`'s ±100,000,000-day range, where `toISOString()` throws.
    expect(displayUptime(8.65e15, NOW)).toBeUndefined();
    expect(displayUptime(-8.65e15, NOW)).toBeUndefined();
    expect(displayUptime(Number.MAX_SAFE_INTEGER, NOW)).toBeUndefined();
  });

  /**
   * The identical clock-skew rule, on the identical boundary — pinned here as
   * well as on `displayActivity` so a future edit that forks the policy fails
   * rather than quietly leaving the two columns disagreeing about the same
   * skew. A spawn instant is stamped by the daemon itself rather than by a hook
   * process, so it is skewed by one clock rather than two; that is a narrower
   * gap inside the same tolerance, not a reason for a second policy.
   */
  it("absorbs ordinary clock skew and refuses to relativise anything beyond it", () => {
    expect(displayUptime(NOW + 1, NOW)?.label).toBe("<1m");
    expect(displayUptime(NOW + CLOCK_SKEW_TOLERANCE_MS, NOW)?.label).toBe("<1m");
    expect(displayUptime(NOW + CLOCK_SKEW_TOLERANCE_MS + 1, NOW)).toBeUndefined();
    expect(displayUptime(NOW + 60 * 60_000, NOW)).toBeUndefined();
    expect(displayUptime(NOW + 3650 * 24 * 60 * 60_000, NOW)).toBeUndefined();
  });

  it("defaults `now` to the real clock, so callers need not pass one", () => {
    expect(displayUptime(Date.now())?.label).toBe("<1m");
  });
});
