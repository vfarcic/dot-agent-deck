import { describe, expect, it } from "vitest";

import { imageName, parseNaming, parseThemes } from "./themes";

/**
 * PR #2491: the docs-screenshot run shoots every scene in light and dark. These
 * are the rasterizer's half of the names `cargo docs-screenshots` checks for
 * (`expected_images` in `xtask/screenshots/src/main.rs`); collected by `pnpm
 * test` (vitest), since the screenshot config runs only `*.shot.ts`.
 */
describe("docs screenshot themes", () => {
  it("shoots both themes unless the run names fewer", () => {
    expect(parseThemes(undefined)).toEqual(["light", "dark"]);
    expect(parseThemes("")).toEqual(["light", "dark"]);
    expect(parseThemes("light,dark")).toEqual(["light", "dark"]);
    expect(parseThemes("dark")).toEqual(["dark"]);
    expect(parseThemes("light, light")).toEqual(["light"]);
    expect(() => parseThemes("light,system")).toThrow(/unknown theme/);
  });

  it("names a pair by theme and a single-theme image by the name the pages already use", () => {
    expect(imageName("dashboard", "desktop", "light", "paired")).toBe("dashboard-desktop-light.png");
    expect(imageName("dashboard", "tui", "dark", "paired")).toBe("dashboard-tui-dark.png");
    expect(imageName("self-upgrade", "tui", "dark", "single")).toBe("self-upgrade-tui.png");
  });

  it("takes single naming only with one theme", () => {
    expect(parseNaming(undefined, ["light", "dark"])).toBe("paired");
    expect(parseNaming("paired", ["light", "dark"])).toBe("paired");
    expect(parseNaming("single", ["dark"])).toBe("single");
    expect(() => parseNaming("single", ["light", "dark"])).toThrow(/exactly one theme/);
    expect(() => parseNaming("both", ["dark"])).toThrow(/paired or single/);
  });
});
