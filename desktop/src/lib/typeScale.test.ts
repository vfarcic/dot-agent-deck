import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { TERMINAL_FONT_SIZE } from "./typeScale";

/**
 * Issue #2312: the app's text is sized from the agent output's size. jsdom
 * loads no stylesheet, so these read `styles.css` itself.
 */
const css = readFileSync(resolve("src/styles.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");

function token(name: string): number {
  const match = css.match(new RegExp(`--${name}:\\s*([\\d.]+)px;`));
  if (!match) throw new Error(`styles.css declares no --${name} in px`);
  return Number(match[1]);
}

const STEPS = ["text-xs", "text-sm", "text-base", "text-lg", "text-xl", "text-2xl"];

describe("the app's type scale", () => {
  it("bases --text-base on the terminal's font size", () => {
    expect(token("text-base")).toBe(TERMINAL_FONT_SIZE);
  });

  it("climbs step by step and never goes below the 11px floor", () => {
    const sizes = STEPS.map(token);
    expect(sizes[0]).toBeGreaterThanOrEqual(11);
    for (let i = 1; i < sizes.length; i += 1) expect(sizes[i], STEPS[i]).toBeGreaterThan(sizes[i - 1]);
  });

  it("sizes every rule from a step rather than a one-off value", () => {
    const sizes = [...css.matchAll(/font-size:\s*([^;}]+)/g)].map((match) => match[1].trim());
    expect(sizes.length).toBeGreaterThan(0);
    const oneOff = sizes.filter((size) => !/^(calc\()?var\(--text-(xs|sm|base|lg|xl|2xl)\)/.test(size));
    expect(oneOff).toEqual([]);
  });
});
