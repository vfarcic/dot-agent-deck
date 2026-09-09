import { expect, test, type Page } from "@playwright/test";

/**
 * The appearance resolves in the engine that ships, not only in Chromium.
 *
 * PRD #743 verified the dark palette by resolving 399 declarations in
 * `chrome-headless-shell` over CDP, and `docs/develop/desktop-gui.md` records
 * what that could not reach: `color-scheme: light dark` on `<meta>` and
 * `theme-color` with a `media` attribute, "the concrete things Chromium cannot
 * answer for WebKitGTK/WKWebView" (issue #823).
 *
 * Under the `webkit` project this file puts the FIRST of those two questions to
 * a WebKit engine, and only that one. The `color-scheme` test below measures an
 * engine-side effect: the document's used colour scheme, read through the
 * `Canvas` system colour against forced-scheme controls. The `theme-color` test
 * cannot, because which `theme-color` an engine resolved is not observable from
 * inside the page in either engine (measured -- the comment on that test has
 * the sweep). Whether an engine honours `media` on `theme-color` therefore
 * remains answered by NOTHING automated, and `docs/develop/desktop-gui.md` says
 * plainly that no step of the manual walk names it either -- closing it needs
 * the real-window rung, issue #953.
 *
 * It is deliberately NOT a re-implementation of the palette comparison. The 399
 * declarations are still checked where they were, and the two static guards in
 * `xtask/linkage-check/src/desktop_palette.rs` still run in the required `build`
 * job. What is here is the handful of properties that need a media query
 * evaluated and a colour composited, in each engine, against the built bundle.
 */

/** sRGB relative luminance, for "is this actually dark" rather than "is it this hex". */
function luminance(color: string): number {
  const parts = color.match(/[\d.]+/g);
  if (!parts || parts.length < 3) throw new Error(`unparseable colour: ${color}`);
  const channel = (raw: string) => {
    const value = Number(raw) / 255;
    return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(parts[0]) + 0.7152 * channel(parts[1]) + 0.0722 * channel(parts[2]);
}

/** `#101514` — the dark `--canvas`, which `index.html` says the dark `theme-color` mirrors. */
const DARK_CANVAS = { r: 16, g: 21, b: 20 };

async function canvasBackground(page: Page): Promise<string> {
  return page.locator(".control-deck").evaluate((node) => getComputedStyle(node).backgroundColor);
}

test.describe("appearance in a real engine", () => {
  test("the meta color-scheme puts the webview's own chrome into dark", async ({ page }) => {
    await page.goto("/?fixture=1");
    await expect(page.getByTestId("open-overview")).toBeVisible();
    await page.emulateMedia({ colorScheme: "dark" });
    await expect
      .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
      .toBe(true);

    /*
      `<meta name="color-scheme" content="light dark">` is what makes the
      webview's OWN chrome — its scrollbars, its form controls, the default
      background behind the page — follow the appearance instead of staying
      light under a dark document. An engine that ignores it leaves the document
      light-only, and the app then paints a dark page inside a light webview.

      It is NOT observable as `getComputedStyle(documentElement).colorScheme`:
      that reads the CSS property, which nothing here declares, so it is
      `normal` in both engines whether the meta works or not. What the meta
      changes is the document's USED colour scheme, and the `Canvas` system
      colour resolves against exactly that. The two forced probes are the
      control: `only light` pins a probe to the light scheme and `only dark` to
      the dark one, so a suite where all three agreed would be measuring the
      media query rather than the meta.
    */
    const probe = await page.evaluate(() => {
      const canvasUnder = (scheme: string) => {
        const el = document.createElement("div");
        el.style.background = "Canvas";
        if (scheme) el.style.colorScheme = scheme;
        document.body.appendChild(el);
        const background = getComputedStyle(el).backgroundColor;
        el.remove();
        return background;
      };
      return { inherited: canvasUnder(""), forcedLight: canvasUnder("only light"), forcedDark: canvasUnder("only dark") };
    });

    expect(luminance(probe.forcedLight)).toBeGreaterThan(0.5);
    expect(luminance(probe.forcedDark)).toBeLessThan(0.1);
    expect(
      probe.inherited,
      "the document's used colour scheme stayed light under a dark preference, so the color-scheme meta did not take effect",
    ).toBe(probe.forcedDark);
  });

  test("prefers-color-scheme repaints the canvas, both ways", async ({ page }) => {
    await page.goto("/?fixture=1");
    await expect(page.getByTestId("open-overview")).toBeVisible();

    await page.emulateMedia({ colorScheme: "light" });
    // The emulation has landed once the engine's own matcher agrees; that is
    // the state to wait on, rather than a timer after `emulateMedia`.
    await expect
      .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
      .toBe(false);
    const light = await canvasBackground(page);

    await page.emulateMedia({ colorScheme: "dark" });
    await expect
      .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
      .toBe(true);
    const dark = await canvasBackground(page);

    expect(dark, "the canvas painted the same colour in both appearances").not.toBe(light);
    expect(luminance(light)).toBeGreaterThan(0.5);
    expect(luminance(dark)).toBeLessThan(0.1);
  });

  /*
    Narrowed in PR #958 after a Greptile finding, and the narrowing is the point
    of the long title: this test does NOT assert that the engine applies `media`
    to `theme-color`, because that is not observable from inside the page.

    `theme-color` tints browser UI chrome, and nothing reports back which meta
    the engine resolved. Swept in both engines of this tier, at the same
    `?fixture=1` page and under an emulated dark preference: no property whose
    name matches /theme/i exists anywhere on the prototype chains of `window`,
    `document`, `navigator` or `screen`; `theme-color` is not a CSS property, so
    `CSS.supports("theme-color", "#101514")` is `false` and
    `getComputedStyle(document.documentElement).getPropertyValue("theme-color")`
    is the empty string; and `HTMLMetaElement.prototype` has exactly six own
    properties in both engines -- `constructor`, `content`, `httpEquiv`,
    `media`, `name`, `scheme` -- none of which reports resolution. Chromium's CDP does have a `themeColor`, but it is the
    web app MANIFEST's `theme_color`, and CDP is Chromium-only in any case — so
    it cannot answer the WebKit question this tier exists for. A previous
    version of this test re-derived the pick with `window.matchMedia(meta.media)`
    and read that as the engine's own selection, which an engine that honours
    `matchMedia` while ignoring `media` on `theme-color` would have passed.

    What IS observable, and all this asserts:
      1. `index.html` carries exactly one `theme-color` per appearance, each
         carrying a `media` attribute. A bare one applies to both appearances
         and silently defeats the pair, so the shape is the assertion.
      2. The engine's own `matchMedia` resolves those two `media` strings the way
         the app assumes, in BOTH directions — so a pair that could never
         separate fails here.
      3. The dark meta's content is still the colour the engine composites for
         `--canvas` in dark. `index.html` says in a comment that the dark
         `theme-color` IS `--canvas`'s dark value from `styles.css`; nothing
         checked it, so the two could drift the next time either is touched.
         This part is a real engine-side measurement — `canvasBackground` reads a
         painted, composited colour — it just says nothing about `theme-color`.
  */
  test("theme-color is one media-scoped meta per appearance, and the dark one still matches the dark canvas — whether the engine applies that media is not observable from the page and is not asserted", async ({
    page,
  }) => {
    await page.goto("/?fixture=1");
    await expect(page.getByTestId("open-overview")).toBeVisible();

    const metas = await page.evaluate(() =>
      Array.from(document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')).map((meta) => ({
        content: meta.content,
        media: meta.getAttribute("media"),
      })),
    );
    expect(metas, "index.html no longer carries exactly one media-scoped theme-color per appearance").toEqual([
      { content: "#f3f0e9", media: "(prefers-color-scheme: light)" },
      { content: "#101514", media: "(prefers-color-scheme: dark)" },
    ]);

    for (const scheme of ["light", "dark"] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      await expect
        .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
        .toBe(scheme === "dark");

      // A bare meta would land in both iterations via the `all` fallback, and a
      // pair that both match or neither match fails the single-element compare.
      const matched = await page.evaluate(() =>
        Array.from(document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]'))
          .filter((meta) => window.matchMedia(meta.getAttribute("media") ?? "all").matches)
          .map((meta) => meta.content),
      );
      expect(
        matched,
        `the engine's matchMedia did not single out one theme-color under a ${scheme} preference`,
      ).toEqual([scheme === "dark" ? "#101514" : "#f3f0e9"]);
    }

    // Left in dark by the loop, but pinned again so the drift guard below does
    // not depend on the loop's iteration order.
    await page.emulateMedia({ colorScheme: "dark" });
    await expect
      .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
      .toBe(true);

    const painted = await canvasBackground(page);
    const parts = (painted.match(/[\d.]+/g) ?? []).map(Number);
    expect({ r: parts[0], g: parts[1], b: parts[2] }).toEqual(DARK_CANVAS);
  });
});
