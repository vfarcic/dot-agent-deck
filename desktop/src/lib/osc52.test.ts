import { describe, expect, it } from "vitest";
import { OSC52_MAX_TEXT_BYTES, osc52CopyText } from "./osc52";

function base64(text: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte);
  return btoa(binary);
}

describe("osc52CopyText", () => {
  it("decodes the copy Claude Code sends", () => {
    expect(osc52CopyText(`c;${base64("hello")}`)).toBe("hello");
  });

  it("decodes UTF-8 text, newlines included", () => {
    expect(osc52CopyText(`c;${base64("naïve — ✓\nnext")}`)).toBe("naïve — ✓\nnext");
  });

  it("accepts every target OSC 52 defines, and the empty default", () => {
    for (const targets of ["", "c", "p", "q", "s", "0", "7", "cp", "s0"]) {
      expect(osc52CopyText(`${targets};${base64("x")}`)).toBe("x");
    }
  });

  it("accepts base64 without its padding", () => {
    expect(osc52CopyText("c;aGk")).toBe("hi");
  });

  it("never treats a read request as a copy", () => {
    expect(osc52CopyText("c;?")).toBeUndefined();
    expect(osc52CopyText(";?")).toBeUndefined();
  });

  it("refuses a malformed request", () => {
    expect(osc52CopyText("c")).toBeUndefined(); // no payload field
    expect(osc52CopyText("c;")).toBeUndefined(); // empty: would clear the clipboard
    expect(osc52CopyText(`z;${base64("x")}`)).toBeUndefined(); // undefined target
    expect(osc52CopyText("c;not*base64!")).toBeUndefined();
    expect(osc52CopyText(`c;${btoa("\xff\xfe")}`)).toBeUndefined(); // not UTF-8
  });

  it("accepts text up to the limit and refuses one byte more", () => {
    expect(osc52CopyText(`c;${base64("a".repeat(OSC52_MAX_TEXT_BYTES))}`)).toHaveLength(OSC52_MAX_TEXT_BYTES);
    expect(osc52CopyText(`c;${base64("a".repeat(OSC52_MAX_TEXT_BYTES + 1))}`)).toBeUndefined();
    expect(osc52CopyText(`c;${"A".repeat(4 * OSC52_MAX_TEXT_BYTES)}`)).toBeUndefined();
  });
});
