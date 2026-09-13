import { expect, test, type Page } from "@playwright/test";

/**
 * The Deck selector, driven the way a person drives it, in both engines
 * (PRD #741 M9).
 *
 * Three of these behaviours are the reason this file is here rather than in the
 * vitest suite, and they are the three `column-picker.spec.ts` names for the
 * same shape of control: dismissal on an outside click is HIT-TESTING, the
 * trigger toggling rather than reopening is EVENT ORDER, and the menu floating
 * over the screen is LAYOUT and STACKING. jsdom computes none of them.
 *
 * The fourth is this control's own: it sits in `.repo-context`, which is the top
 * bar's first grid column at `minmax(210px, 1fr)`, so a menu that laid out in
 * flow would push the instruments and the actions sideways. That is a question
 * about the page's width and only an engine can answer it — asserted at 900px,
 * the narrow viewport `page-overflow.spec.ts` already established as the one
 * where this app's top bar is actually under pressure.
 *
 * And a fifth that is about engines rather than about layout: `#1032`. The house
 * settings-row convention floats a `<legend>` to make it a grid item, and WebKit
 * forces a rendered legend's `float` to `none` — so the form works in Chromium
 * and collapses on the engine the app ships on. This surface is new, so it uses
 * a `<span id>` plus `aria-labelledby` instead, and the check that it does runs
 * in both projects.
 *
 * Nothing here needs a daemon or a credential: the browser preview keeps its
 * settings document in `localStorage`, so a spec can seed the decks it wants.
 */

/** `FIXTURE_SETTINGS_KEY` — deliberately unscoped, because a theme is global. */
const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

const BUILD_BOX = "a1b2c3d4e5f60718";
const RELAY = "0f1e2d3c4b5a6978";

/** Two stored decks, so the list has something in it that is not the local one. */
const TWO_DECKS = {
  remote: [
    { host: "build-box.example.com", id: BUILD_BOX, port: 22, user: "vf", socket: "/run/deck.sock" },
    { host: "relay.example.com", id: RELAY, port: 2222 },
  ],
  selection: "local",
};

/**
 * Seed the preview's settings document before the bundle runs — **only when
 * there is none**.
 *
 * The guard is load-bearing rather than tidy. `addInitScript` runs before every
 * navigation, a reload included, so an unconditional seed would overwrite the
 * choice the test had just made and then assert that it did not survive — a test
 * that fails whatever the app does.
 */
async function seed(page: Page, endpoints?: unknown) {
  await page.addInitScript(
    ([key, document]) => {
      if (window.localStorage.getItem(key as string)) return;
      window.localStorage.setItem(key as string, JSON.stringify(document));
    },
    [SETTINGS_KEY, { version: 1, appearance: { mode: "light" }, zoom: { level: 1 }, endpoints }] as const,
  );
}

/** The `selection` token in the preview's stored document, as written. */
async function storedSelection(page: Page): Promise<string | undefined> {
  return page.evaluate((key) => {
    const raw = window.localStorage.getItem(key);
    if (!raw) return undefined;
    return (JSON.parse(raw) as { endpoints?: { selection?: string } }).endpoints?.selection;
  }, SETTINGS_KEY);
}

/** The deck screen, with the selector on screen. */
async function openDeck(page: Page, endpoints: unknown = TWO_DECKS, scenario = "crowded") {
  await seed(page, endpoints);
  await page.goto(`/?fixture=1&state=${scenario}`);
  await expect(page.getByTestId("deck-selector-toggle")).toBeVisible();
}

/** The overview, with the selector on screen. */
async function openOverviewScreen(page: Page, endpoints: unknown = TWO_DECKS) {
  await seed(page, endpoints);
  await page.goto("/?fixture=1&state=crowded");
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("overview-table-region")).toBeVisible();
  await expect(page.getByTestId("deck-selector-toggle")).toBeVisible();
}

async function openMenu(page: Page) {
  await page.getByTestId("deck-selector-toggle").click();
  const menu = page.getByTestId("deck-selector-menu");
  await expect(menu).toBeVisible();
  return menu;
}

/** Every page-width metric in one round trip, so nothing reflows between reads. */
async function pageMetrics(page: Page) {
  return page.evaluate(() => ({
    rootScroll: document.documentElement.scrollWidth,
    rootClient: document.documentElement.clientWidth,
    bodyScroll: document.body.scrollWidth,
    bodyClient: document.body.clientWidth,
    viewport: window.innerWidth,
  }));
}

/** The same contract `page-overflow.spec.ts` asserts, on both the body and the root. */
function expectNoPageOverflow(metrics: Awaited<ReturnType<typeof pageMetrics>>) {
  expect(metrics.bodyScroll, "content extends past the body, clipped by overflow-x: hidden").toBeLessThanOrEqual(metrics.bodyClient);
  expect(metrics.rootScroll, "the root scrolls sideways, so the whole page slides").toBeLessThanOrEqual(metrics.rootClient);
  expect(metrics.rootClient, "the root is wider than the viewport").toBeLessThanOrEqual(metrics.viewport);
}

/** The two shells the selector appears on, driven identically. */
const SCREENS = [
  { name: "the deck", open: openDeck },
  { name: "the overview", open: openOverviewScreen },
] as const;

for (const screen of SCREENS) {
  test.describe(`the Deck selector on ${screen.name}`, () => {
    test("lists every configured deck and defaults to the local one", async ({ page }) => {
      await screen.open(page);
      await expect(page.getByTestId("deck-selector-current")).toHaveText("This machine");

      const menu = await openMenu(page);
      await expect(menu.getByRole("radio")).toHaveText([
        "This machine",
        "vf@build-box.example.com",
        "relay.example.com:2222",
      ]);
      await expect(page.getByTestId("deck-selector-option-local")).toHaveAttribute("aria-checked", "true");
    });

    test("switching names the chosen deck and survives a reload", async ({ page }) => {
      await screen.open(page);
      const menu = await openMenu(page);

      await menu.getByTestId(`deck-selector-option-${BUILD_BOX}`).click();

      await expect(menu).toBeHidden();
      await expect(page.getByTestId("deck-selector-current")).toHaveText("vf@build-box.example.com");

      /*
        The whole of "switching re-renders the fleet" that a browser can see: the
        choice goes through `saveSettings`, which in the preview is the
        `localStorage` document and in the app is `desktop_set_settings` →
        `apply_selection` — the one path that drops the links, releases the other
        tunnels, restarts the watcher's subscription and emits the new deck's
        snapshot. What can be checked here is that the document was WRITTEN with
        the chosen token rather than the choice being held in React state, and
        that a reload comes back on the chosen deck.
      */
      await expect.poll(() => storedSelection(page)).toBe(BUILD_BOX);
      await page.reload();
      await expect(page.getByTestId("deck-selector-current")).toHaveText("vf@build-box.example.com");
    });

    test("opens under its trigger, floats over the screen, and toggles rather than reopening", async ({ page }) => {
      await screen.open(page);
      const toggle = page.getByTestId("deck-selector-toggle");
      await expect(toggle).toHaveAttribute("aria-expanded", "false");

      await openMenu(page);
      await expect(toggle).toHaveAttribute("aria-expanded", "true");

      // Everything in one `evaluate`: both screens re-render on their own clock,
      // and two round trips can straddle one.
      const geometry = await page.evaluate(() => {
        const menu = document.querySelector('[data-testid="deck-selector-menu"]')!;
        const trigger = document.querySelector('[data-testid="deck-selector-toggle"]')!;
        const topbar = document.querySelector(".topbar")!;
        const box = menu.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        return {
          box: { top: box.top, bottom: box.bottom, left: box.left, right: box.right, width: box.width, height: box.height },
          triggerBottom: trigger.getBoundingClientRect().bottom,
          topbarBottom: topbar.getBoundingClientRect().bottom,
          viewportWidth: window.innerWidth,
          hitIsInsideMenu: hit !== null && menu.contains(hit),
          hitTag: hit?.tagName ?? "nothing",
        };
      });

      expect(geometry.box.width, "the menu laid out with no width").toBeGreaterThan(0);
      expect(geometry.box.height, "the menu laid out with no height").toBeGreaterThan(0);
      expect(geometry.box.top).toBeGreaterThanOrEqual(geometry.triggerBottom);
      expect(geometry.box.left).toBeGreaterThanOrEqual(0);
      expect(geometry.box.right).toBeLessThanOrEqual(geometry.viewportWidth);
      // Below the top bar, over the content — only possible out of flow. In flow
      // it would be clipped inside a 72px bar with most options unreachable.
      expect(geometry.box.bottom, "the menu did not extend past the top bar, so it is not floating").toBeGreaterThan(geometry.topbarBottom);
      // A real hit test at the menu's own centre: `z-index: 30` is what puts it
      // in front of the content it covers.
      expect(geometry.hitIsInsideMenu, `a click at the menu's centre would land on <${geometry.hitTag}>`).toBe(true);

      /*
        The close-then-reopen trap. The dismiss listener fires on `pointerdown`
        and the trigger on `click`, so ONE press produces both in that order —
        and were the trigger outside the picker's root, the press would close the
        menu on the way down and the release would reopen it, leaving a button
        that looks dead. Playwright dispatches the real sequence.
      */
      await toggle.click();
      await expect(page.getByTestId("deck-selector-menu"), "a second press left the menu open").toBeHidden();
      await expect(toggle).toHaveAttribute("aria-expanded", "false");

      await toggle.click();
      await expect(page.getByTestId("deck-selector-menu")).toBeVisible();
    });

    test("groups its options with a span rather than a legend (issue 1032)", async ({ page }) => {
      await screen.open(page);
      const menu = await openMenu(page);

      /*
        The engine's own answer, not the markup's. `#1032` is about a rendered
        `<legend>` whose `float` WebKit forces to `none` — so the check that
        matters is that the group's accessible name comes from an element this
        control positions itself, and that there is no fieldset in the subtree
        for an engine to have an opinion about.
      */
      await expect(menu.getByRole("radiogroup")).toHaveAccessibleName("Deck");
      expect(await menu.locator("legend").count()).toBe(0);
      expect(await menu.locator("fieldset").count()).toBe(0);
    });
  });
}

test.describe("the Deck selector's state line", () => {
  test("names an unreachable deck's problem without blanking the screen", async ({ page }) => {
    // The `error` fixture is a daemon that answered and was refused, which is
    // the state a wrong or an incompatible deck arrives in.
    await openDeck(page, { ...TWO_DECKS, selection: BUILD_BOX }, "error");

    const state = page.getByTestId("deck-selector-state");
    await expect(state).toBeVisible();
    expect((await state.innerText()).trim().length, "the state line rendered no text").toBeGreaterThan(0);

    // Not blanked: the deck is still named, the selector still opens, and the
    // rest of the shell is still there.
    await expect(page.getByTestId("deck-selector-current")).toHaveText("vf@build-box.example.com");
    const menu = await openMenu(page);
    await expect(menu.getByRole("radio")).toHaveCount(3);
    await expect(page.getByTestId("open-overview")).toBeVisible();
  });
});

test.describe("at 900x800 narrow", () => {
  test.use({ viewport: { width: 900, height: 800 } });

  test("the selector does not push the page sideways, open or closed", async ({ page }) => {
    await openDeck(page, { ...TWO_DECKS, selection: BUILD_BOX });

    expectNoPageOverflow(await pageMetrics(page));

    const menu = await openMenu(page);
    expectNoPageOverflow(await pageMetrics(page));

    // And the menu itself is wholly on screen at this width, which is the thing
    // a max-width and a left-aligned absolute box are there to guarantee.
    const box = (await menu.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(900);
  });

  test("the overview keeps the same guarantee", async ({ page }) => {
    await openOverviewScreen(page);
    expectNoPageOverflow(await pageMetrics(page));
    await openMenu(page);
    expectNoPageOverflow(await pageMetrics(page));
  });
});
