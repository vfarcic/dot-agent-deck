import { expect, test, type Page } from "@playwright/test";

/**
 * The appearance resolves in the engine that ships, not only in Chromium.
 *
 * PRD #743 verified the dark palette by resolving 399 declarations in
 * `chrome-headless-shell` over CDP, and `docs/develop/desktop-gui.md` records
 * what that could not reach: `color-scheme: light dark` on `<meta>` and
 * `theme-color` with a `media` attribute, "the concrete things Chromium cannot
 * answer for WebKitGTK/WKWebView" (issue #823). Under the `webkit` project this
 * file runs those two questions through a WebKit engine.
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

  test("the dark theme-color still matches the dark canvas it was copied from", async ({ page }) => {
    await page.goto("/?fixture=1");
    await expect(page.getByTestId("open-overview")).toBeVisible();
    await page.emulateMedia({ colorScheme: "dark" });
    await expect
      .poll(() => page.evaluate(() => window.matchMedia("(prefers-color-scheme: dark)").matches))
      .toBe(true);

    // `index.html` carries one `theme-color` per appearance and says in a
    // comment that the dark one IS `--canvas`'s dark value. Nothing checked
    // that, so the two could drift the next time either is touched. The engine
    // picks the meta by evaluating its own `media` attribute — the `media`
    // support #823 names as the part that varies between engines — and the
    // canvas colour is the composited value the engine actually painted.
    const chosen = await page.evaluate(() =>
      Array.from(document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]'))
        .filter((meta) => !meta.media || window.matchMedia(meta.media).matches)
        .map((meta) => meta.content),
    );
    expect(chosen, "no theme-color matched the dark appearance").toEqual(["#101514"]);

    const painted = await canvasBackground(page);
    const parts = (painted.match(/[\d.]+/g) ?? []).map(Number);
    expect({ r: parts[0], g: parts[1], b: parts[2] }).toEqual(DARK_CANVAS);
  });
});
