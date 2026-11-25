import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * PR #2491 decision 2026-11-25 — the card overlay is gone: a card opens its
 * agent's pane, whose Card panel shows the card beside the terminal (above it
 * in a narrow window). Driven in a real browser against the fixture scene
 * `/?fixture=1&state=docs&cards=tree`, whose daemon lists an agent for each of
 * its cards and lets a test push a changed card list (`__dadFixtureCards`).
 *
 * - a click on a card's body opens the agent's pane with the Card panel, on
 *   the right of the terminal at 1440 px and between the header and the
 *   terminal at 375 px, the page never scrolling sideways; the panel neither
 *   opens nor closes nor changes the terminal's size when the card changes,
 *   and the header's Card button hides and shows it (097);
 * - the header's previous, next and Next needing you move between agents, and
 *   `[`, `]` and `n` do the same unless the keyboard is in the terminal, where
 *   they reach the agent and move nothing (098);
 * - the Table view's row menu: Show card opens the pane with its panel; the
 *   agent's most needed action opens the pane under its review and sends
 *   nothing (099);
 * - every action icon, on a card, in the selection bar and in the panel, shows
 *   a mark under the pointer and a ring under the keyboard in both themes, and
 *   its tooltip sits just below or above it (100).
 */

const TOLERANCE = 1;

interface RecordingWindow {
  __dadE2eTerminals?: { element?: HTMLElement; onData(listener: (data: string) => void): unknown }[];
  __dadE2eSent?: string[];
}

/** Keep every xterm the app builds, so a test can listen to what is typed into one. */
async function captureTerminals(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const originalSet = Map.prototype.set;
    const terminals: unknown[] = [];
    Object.defineProperty(window, "__dadE2eTerminals", { value: terminals });
    Object.defineProperty(Map.prototype, "set", {
      configurable: true,
      writable: true,
      value(this: Map<unknown, unknown>, key: unknown, value: unknown) {
        if (value && typeof value === "object") {
          const candidate = value as { element?: unknown; onData?: unknown };
          if (candidate.element instanceof HTMLElement && typeof candidate.onData === "function") terminals.push(value);
        }
        return Reflect.apply(originalSet, this, [key, value]);
      },
    });
  });
}

async function openTree(page: Page, mode: "cards" | "table" = "cards"): Promise<void> {
  await page.addInitScript((shown) => {
    try {
      window.localStorage.setItem("dot-agent-deck.overview.mode", shown);
      window.localStorage.removeItem("dot-agent-deck.cards.open");
      window.localStorage.removeItem("dot-agent-deck.agent-pane.card-panel");
    } catch {
      // A blocked storage leaves the defaults.
    }
  }, mode);
  await page.goto("/?fixture=1&state=docs&cards=tree");
  await page.getByTestId("open-overview").click();
  if (mode === "cards") await expect(page.getByTestId("agent-cards-tree")).toBeVisible();
}

async function pushCards(page: Page, change: string): Promise<void> {
  await page.evaluate((body) => {
    type Entry = Record<string, unknown> & { name: string };
    const hook = (window as unknown as { __dadFixtureCards: { snapshot(): { revision: number; entries: Entry[] }; push(next: unknown): void } }).__dadFixtureCards;
    const next = hook.snapshot();
    const byName = (name: string) => {
      const found = next.entries.find((entry) => entry.name === name);
      if (!found) throw new Error(`no entry named ${name}`);
      return found;
    };
    new Function("byName", "next", body)(byName, next);
    next.revision += 1;
    hook.push(next);
  }, change);
}

async function keyOf(page: Page, name: string): Promise<string> {
  return page.evaluate((wanted) => {
    const hook = (window as unknown as { __dadFixtureCards: { snapshot(): { entries: { name: string; key: string }[] } } }).__dadFixtureCards;
    return hook.snapshot().entries.find((entry) => entry.name === wanted)!.key;
  }, name);
}

async function cardNamed(page: Page, name: string): Promise<Locator> {
  return page.locator(`article[data-card-key="${await keyOf(page, name)}"]`);
}

/** A catalog prompt action, offered on every card of the scene. */
const OFFER = `
  const offered = (id, label, icon) => ({ id, label, kind: "prompt", prompt: "PANEL-2491 " + id + ": do it.", prompt_source: "catalog", text_input: false,
    confirm: false, confirm_detail: null, icon, placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-panel-" + id });
  for (const entry of next.entries) entry.actions = [offered("p1", "Go ahead", "play"), offered("p2", "Merge it", "git-merge"), ...entry.actions];
`;

const box = async (locator: Locator) => (await locator.boundingBox())!;

async function expectNoSidewaysScroll(page: Page): Promise<void> {
  const metrics = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(metrics.scroll, "the page scrolls sideways").toBeLessThanOrEqual(metrics.client);
}

/** The shown tooltip sits just below or just above `icon`, starting over it. */
async function expectTipBesideIcon(page: Page, icon: Locator, what: string): Promise<void> {
  const tip = await box(page.getByRole("tooltip"));
  const at = await box(icon);
  const below = Math.abs(tip.y - (at.y + at.height));
  const above = Math.abs(at.y - (tip.y + tip.height));
  expect(Math.min(below, above), `${what}: the tooltip touches its icon's edge (below ${below}, above ${above})`).toBeLessThanOrEqual(10);
  expect(tip.x, `${what}: the tooltip does not start right of its icon`).toBeLessThanOrEqual(at.x + at.width);
  expect(tip.x + tip.width, `${what}: the tooltip does not end left of its icon`).toBeGreaterThanOrEqual(at.x);
}

for (const width of [375, 1440] as const) {
  test.describe(`desktop/agent-cards/097 — a card opens its agent's pane with the Card panel at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    /// Scenario: At this width, click the bench card's body. Bench's pane opens with its Card panel and a header whose name, Card, moves and Close agent all stay in the window, on the terminal's right at 1440 px, about 320 px wide, and between the pane's header and the terminal at 375 px with a capped height that scrolls; nothing scrolls sideways. The card then changes — a new summary, coming to need you — and neither the panel nor the terminal's size moves; the header's Card button hides the panel, giving the terminal its room, and shows it again.
    test("the Card panel sits beside or above the terminal, stays put on updates, and the Card button toggles it", async ({ page }) => {
      await openTree(page);
      await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
      const pane = page.getByTestId("agent-pane-overlay");
      await expect(pane).toBeVisible();
      const panel = pane.getByTestId("agent-card-panel");
      await expect(panel).toBeVisible();
      await expect(panel).toHaveAttribute("data-card-key", await keyOf(page, "bench"));
      await expect(pane.getByTestId("agent-pane-card-toggle")).toHaveAttribute("aria-pressed", "true");
      const terminal = pane.locator(".agent-panel");
      const header = pane.locator(".agent-header");
      const [p, t, h] = [await box(panel), await box(terminal), await box(header)];
      if (width === 1440) {
        expect(p.x, "the panel is on the terminal's right").toBeGreaterThanOrEqual(t.x + t.width - TOLERANCE);
        expect(p.width).toBeGreaterThanOrEqual(300);
        expect(p.width).toBeLessThanOrEqual(340);
      } else {
        expect(p.y, "the panel is under the header").toBeGreaterThanOrEqual(h.y + h.height - TOLERANCE);
        expect(p.y + p.height, "the panel is above the terminal").toBeLessThanOrEqual(t.y + TOLERANCE);
        expect(p.height, "the panel's height is fixed and capped").toBeLessThanOrEqual(262);
        expect(await panel.evaluate((node) => getComputedStyle(node).overflowY)).toBe("auto");
        expect(p.width, "the panel spans the pane").toBeGreaterThan(t.width - 2);
      }
      // The header's controls wrap rather than push the agent's name or Close agent out of the pane.
      await expect(header.getByText("bench", { exact: true }).first()).toBeInViewport({ ratio: 1 });
      for (const name of ["Close agent", "Back to dashboard", "Previous agent", "Next agent"]) await expect(pane.getByRole("button", { name, exact: true })).toBeInViewport({ ratio: 1 });
      await expect(pane.getByTestId("agent-pane-next-needs-you")).toBeInViewport({ ratio: 1 });
      await expectNoSidewaysScroll(page);

      // The card changes while the pane is shown: nothing opens, closes or resizes.
      await pushCards(page, `
        const bench = byName("bench");
        bench.needs_you = !bench.needs_you;
        bench.needs_you_reasons = bench.needs_you ? ["question"] : [];
        bench.report = { source: "agent", status: "waiting", summary: "PANEL-UPDATE-2491", published_at: "2026-11-25T00:00:00Z", revision: 9 };
      `);
      await expect(panel).toContainText("PANEL-UPDATE-2491");
      const after = await box(terminal);
      for (const side of ["x", "y", "width", "height"] as const) expect(Math.abs(after[side] - t[side]), `the terminal's ${side}`).toBeLessThanOrEqual(TOLERANCE);

      // The Card button hides the panel, and the terminal takes its room; pressed again, it comes back.
      await pane.getByTestId("agent-pane-card-toggle").click();
      await expect(panel).toHaveCount(0);
      const roomy = await box(terminal);
      expect(width === 1440 ? roomy.width : roomy.height, "the terminal grows").toBeGreaterThan((width === 1440 ? t.width : t.height) + 100);
      await pane.getByTestId("agent-pane-card-toggle").click();
      await expect(pane.getByTestId("agent-card-panel")).toBeVisible();
      await expectNoSidewaysScroll(page);
    });
  });
}

test.describe("desktop/agent-cards/098 — the pane's header moves between agents", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  /// Scenario: Open bench's pane from its card. Next agent shows another agent's pane and card, Previous agent comes back to bench, and Next needing you lands on an agent whose card needs you. With the keyboard on the header, `]` and `[` do the same; with the keyboard in the terminal, typing `]`, `[` and `n` reaches the agent and moves nothing.
  test("previous, next and Next needing you, by button and by key, and the keys stay the agent's in its terminal", async ({ page }) => {
    await captureTerminals(page);
    await openTree(page);
    await pushCards(page, `byName("docs-site").needs_you = true; byName("docs-site").needs_you_reasons = ["question"];`);
    const bench = await keyOf(page, "bench");
    await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
    const pane = page.getByTestId("agent-pane-overlay");
    const shown = () => pane.getByTestId("agent-card-panel").getAttribute("data-card-key");
    await expect(pane.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", bench);
    await pane.getByRole("button", { name: "Next agent" }).click();
    await expect.poll(shown).not.toBe(bench);
    const next = await shown();
    await pane.getByRole("button", { name: "Previous agent" }).click();
    await expect.poll(shown).toBe(bench);
    await pane.getByTestId("agent-pane-next-needs-you").click();
    await expect.poll(async () => pane.getByTestId("agent-card-panel").getByTestId("agent-card-panel-needs-you").count()).toBe(1);

    // The keys, with the keyboard on the header.
    await pane.getByRole("button", { name: "Previous agent" }).click();
    await pane.getByTestId("agent-pane-card-toggle").focus();
    await page.keyboard.press("[");
    const twoBack = await shown();
    await page.keyboard.press("]");
    await expect.poll(shown).not.toBe(twoBack);
    await page.keyboard.press("Escape");
    await expect(pane).toHaveCount(0);

    // In the terminal, the same keys are the agent's.
    await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
    const opened = page.getByTestId("agent-pane-overlay");
    const textarea = opened.locator("textarea.xterm-helper-textarea");
    await textarea.focus();
    await opened.evaluate((root) => {
      const recording = window as Window & RecordingWindow;
      const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
      if (!terminal) throw new Error("the pane's xterm was not captured");
      recording.__dadE2eSent = [];
      terminal.onData((data) => recording.__dadE2eSent?.push(data));
    });
    for (const key of ["]", "[", "n"]) await page.keyboard.press(key);
    await expect(opened.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", bench);
    expect(await page.evaluate(() => (window as Window & RecordingWindow).__dadE2eSent?.join(""))).toBe("][n");
    expect(next).not.toBe(bench);
  });
});

test.describe("desktop/agent-cards/099 — the Table view's row menu opens the agent's pane", () => {
  for (const width of [375, 1440] as const) {
    /// Scenario: At this width, in the Table view, open bench's row menu and press Show card: bench's pane opens with its Card panel. Back on the dashboard, its menu's most needed action — an action that asks first — opens bench's pane under the action's review, showing its exact text, and nothing is sent until Send.
    test(`Show card and the most needed action at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await openTree(page, "table");
      await pushCards(page, `
        const bench = byName("bench");
        bench.actions = [{ id: "ship", label: "Ship it", kind: "prompt", prompt: "TABLE-2491: ship it.", prompt_source: "catalog", text_input: false, confirm: true, confirm_detail: null, icon: "rocket",
          placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-ship" }, ...bench.actions];
        bench.primary_action_id = "ship";
      `);
      const row = page.locator("[data-testid^='overview-agent-']").filter({ hasText: "bench" }).first();
      await row.getByTestId("overview-row-menu").click();
      await row.getByTestId("overview-row-show-card").click();
      const pane = page.getByTestId("agent-pane-overlay");
      await expect(pane.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", await keyOf(page, "bench"));
      await pane.getByRole("button", { name: "Back to dashboard" }).click();
      await expect(pane).toHaveCount(0);

      await row.getByTestId("overview-row-menu").click();
      await row.getByTestId("overview-row-primary").click();
      const review = page.getByTestId("agent-card-confirm");
      await expect(review).toBeVisible();
      await expect(review.getByTestId("agent-card-confirm-prompt")).toHaveText("TABLE-2491: ship it.");
      await expect(page.getByTestId("agent-pane-overlay").getByTestId("agent-card-panel")).toHaveCount(1);
      await expect(page.getByTestId("agent-card-action-result")).toHaveCount(0);
      await review.getByTestId("agent-card-confirm-send").click();
      await expect(review.getByTestId("agent-card-action-result")).toBeVisible();
      await expectNoSidewaysScroll(page);
    });
  }
});

test.describe("desktop/agent-cards/100 — an action icon shows the pointer and the keyboard, and its tooltip sits by it", () => {
  for (const width of [375, 1440] as const) {
    /// Scenario: At this width, in the light and the dark theme, hover and keyboard-focus an action icon on a card, in the selection bar and in the Card panel. Under the pointer the icon is drawn differently than at rest (a filled, outlined button); under the keyboard it has a ring; and its tooltip sits just below or above the icon in each place.
    test(`hover marks and focus rings on the card, the bar and the panel at ${width}px`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await openTree(page);
      await pushCards(page, `next.batch = true; ${OFFER}`);
      const card = await cardNamed(page, "bench");
      const looks = (icon: Locator) => icon.evaluate((node) => {
        const style = getComputedStyle(node);
        return { background: style.backgroundColor, shadow: style.boxShadow, outline: style.outlineStyle, outlineWidth: parseFloat(style.outlineWidth) };
      });
      const check = async (icon: Locator, where: string) => {
        await page.mouse.move(0, 0);
        await icon.evaluate((node) => (node as HTMLElement).blur());
        const rest = await looks(icon);
        await icon.hover();
        await expect(page.getByRole("tooltip")).toBeVisible();
        const hovered = await looks(icon);
        expect(hovered.background !== rest.background || hovered.shadow !== rest.shadow, `${where}: the hovered icon is marked (${JSON.stringify(rest)} → ${JSON.stringify(hovered)})`).toBe(true);
        await expectTipBesideIcon(page, icon, `${where} by pointer`);
        await page.mouse.move(0, 0);
        // The keyboard was used last, so focus shows the keyboard's ring.
        await page.keyboard.press("Shift");
        await icon.focus();
        await expect(icon).toBeFocused();
        expect(await icon.evaluate((node) => node.matches(":focus-visible")), `${where}: focus is the keyboard's`).toBe(true);
        const focused = await looks(icon);
        expect(focused.outline, `${where}: the keyboard's ring`).not.toBe("none");
        expect(focused.outlineWidth, `${where}: the ring is drawn`).toBeGreaterThanOrEqual(2);
        await expectTipBesideIcon(page, icon, `${where} by keyboard`);
        await icon.evaluate((node) => (node as HTMLElement).blur());
      };
      for (const colorScheme of ["light", "dark"] as const) {
        await page.emulateMedia({ colorScheme });
        await page.evaluate(() => document.documentElement.removeAttribute("data-theme"));
        await check(card.getByTestId("agent-card-action-icon").first(), `${colorScheme} card`);
        await card.getByTestId("agent-card-select").check();
        await check(page.getByTestId("agent-cards-selection-bar").getByTestId("action-icon-group").getByRole("button").first(), `${colorScheme} bar`);
        await card.getByTestId("agent-card-details").click();
        const panel = page.getByTestId("agent-card-panel");
        await check(panel.getByTestId("agent-card-action-icon").first(), `${colorScheme} panel`);
        await page.getByTestId("agent-pane-overlay").getByRole("button", { name: "Back to dashboard" }).click();
        await card.getByTestId("agent-card-select").uncheck();
      }
      await expectNoSidewaysScroll(page);
    });
  }
});
