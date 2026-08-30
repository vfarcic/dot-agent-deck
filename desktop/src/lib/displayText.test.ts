import { describe, expect, it } from "vitest";
import { clampText, DISPLAY_LIMITS, displayPath, displayText, homeRelative, sanitizeText, shortDaemonLabel } from "./displayText";

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

describe("shortDaemonLabel", () => {
  it("names the daemon without printing the uid or username in its socket path", () => {
    expect(shortDaemonLabel("/run/user/1000/dot-agent-deck/daemon.sock")).toBe("daemon.sock");
    expect(shortDaemonLabel("/tmp/dot-agent-deck.sock")).toBe("dot-agent-deck.sock");
    expect(shortDaemonLabel("/home/dev/.local/state/dot-agent-deck/deck.sock")).toBe("deck.sock");
  });

  it("falls back to the whole value when there is no segment to take", () => {
    expect(shortDaemonLabel("dot-agent-deck.sock")).toBe("dot-agent-deck.sock");
    expect(shortDaemonLabel("/")).toBe("/");
  });
});
