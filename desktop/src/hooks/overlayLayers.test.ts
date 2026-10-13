import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { OVERLAY_LAYER } from "./useInertBackground";

/**
 * PR #1680 review — `OVERLAY_LAYER` is the order voice's "close", the fence and
 * the pull request browser's "covered" read the app's overlays in, and it is
 * only true while it matches the order the stylesheet paints them in. jsdom
 * loads no stylesheet, so this reads the numbers back from `styles.css` itself.
 */
describe("the overlay layers", () => {
  const css = readFileSync(resolve("src/styles.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");

  /** The `z-index` the last rule whose selector list is exactly `selector` sets. */
  function zIndex(selector: string): number | undefined {
    let found: number | undefined;
    for (const match of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      const selectors = match[1].split(",").map((part) => part.trim());
      const z = /(?:^|;)\s*z-index:\s*(\d+)/.exec(match[2]);
      if (z && selectors.includes(selector)) found = Number(z[1]);
    }
    return found;
  }

  /** Scenario: each layer the stack orders overlays by is the z-index its backdrop is painted at. */
  it.each([
    [".sheet-backdrop", OVERLAY_LAYER.sheet],
    [".agent-pane-overlay", OVERLAY_LAYER.pane],
    [".pr-browser-overlay", OVERLAY_LAYER.pullRequest],
    [".sheet-backdrop.is-raised", OVERLAY_LAYER.raisedSheet],
    [".dialog-backdrop", OVERLAY_LAYER.dialog],
  ])("paints %s at its layer", (selector, layer) => {
    expect(zIndex(selector)).toBe(layer);
  });

  /** Scenario: a raised sheet is drawn above the pane and the browser, and beneath every dialog. */
  it("raises a sheet between the browser and the dialogs", () => {
    expect(OVERLAY_LAYER.raisedSheet).toBeGreaterThan(OVERLAY_LAYER.pullRequest);
    expect(OVERLAY_LAYER.pullRequest).toBeGreaterThan(OVERLAY_LAYER.pane);
    expect(OVERLAY_LAYER.dialog).toBeGreaterThan(OVERLAY_LAYER.raisedSheet);
  });
});
