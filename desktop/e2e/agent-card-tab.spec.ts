import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * PR #2491 decisions 2026-11-25 and 2026-11-26 — the card overlay is gone,
 * and so is the Card panel that replaced it: a card opens its agent's pane on
 * its Terminal tab, and the pane's Card tab shows the card in the terminal's
 * place. Driven in a real browser against the fixture scene
 * `/?fixture=1&state=docs&cards=tree`, whose daemon lists an agent for each of
 * its cards and lets a test push a changed card list (`__dadFixtureCards`).
 *
 * - a click on a card's body opens the agent's pane on Terminal with a Card
 *   tab and no Card button; the Card tab fills the terminal's place at 375 and
 *   1440 px, the page never scrolling sideways; switching tabs, and the card
 *   changing, never move or resize the terminal, and the agent's xterm is
 *   never resized; the Card tab gains its bell while the agent needs you (097);
 * - the header's previous, next and Next needing you move between agents,
 *   each opening on Terminal, and `[`, `]` and `n` do the same unless the
 *   keyboard is in the terminal, where they reach the agent and move nothing
 *   (098);
 * - the Table view's row menu: Show card opens the pane on Terminal; the
 *   agent's most needed action opens the pane on Terminal under its review
 *   and sends nothing (099);
 * - every action icon, on a card, in the selection bar and in the Card tab,
 *   shows a mark under the pointer and a ring under the keyboard in both
 *   themes, and its tooltip sits just below or above it (100);
 * - a shown Card tab stays while the agent's daemon stops answering and comes
 *   back (105);
 * - a kept card's read-only pane opens on Card, stays through its daemon
 *   reconnecting with the card, and closes once the reconnected daemon lists
 *   its cards without it (104).
 */

const TOLERANCE = 1;

interface RecordingWindow {
  __dadE2eTerminals?: { element?: HTMLElement; cols?: number; rows?: number; onData(listener: (data: string) => void): unknown; onResize?(listener: (size: { cols: number; rows: number }) => void): unknown }[];
  __dadE2eSent?: string[];
  __dadE2eResized?: { cols: number; rows: number }[];
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

/** The pane's tab strip: its Card tab, and whichever tab is shown. */
const cardTabOf = (pane: Locator) => pane.getByRole("tab", { name: /^Card/ });
const shownTabOf = (pane: Locator) => pane.getByRole("tab", { selected: true });

/** Record every size the pane's xterm is given from now on, and its size now. */
async function watchResizes(pane: Locator): Promise<{ cols: number; rows: number }> {
  return pane.evaluate((root) => {
    const recording = window as Window & RecordingWindow;
    const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
    if (!terminal) throw new Error("the pane's xterm was not captured");
    recording.__dadE2eResized = [];
    terminal.onResize?.((size) => recording.__dadE2eResized?.push(size));
    return { cols: terminal.cols ?? 0, rows: terminal.rows ?? 0 };
  });
}

for (const width of [375, 1440] as const) {
  test.describe(`desktop/agent-cards/097 — a card opens its agent's pane on Terminal, with a Card tab, at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    /// Scenario: At this width, click the bench card's body. Bench's pane opens on its Terminal tab, with a Card tab beside it, and a header whose name, moves and Close agent all stay in the window, with no Card button; nothing scrolls sideways. Switch to the Card tab: bench's card fills the terminal's place, scrolling on its own, and the terminal under it keeps its box and its size. The card then changes — a new summary, coming to need you — and nothing switches, moves or resizes; the Card tab gains its bell and reads "Card, needs you". Back on Terminal, the terminal is exactly where and as big as it was, and the agent's terminal was never resized.
    test("the Card tab sits in the terminal's place, and switching tabs never resizes the terminal", async ({ page }) => {
      await captureTerminals(page);
      await openTree(page);
      await pushCards(page, `const bench = byName("bench"); bench.needs_you = false; bench.needs_you_reasons = [];`);
      await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
      const pane = page.getByTestId("agent-pane-overlay");
      await expect(pane).toBeVisible();
      await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
      await expect(cardTabOf(pane)).toHaveAccessibleName("Card");
      await expect(pane.getByTestId("agent-card-panel")).toHaveCount(0);
      await expect(pane.getByTestId("agent-pane-card-toggle")).toHaveCount(0);
      await expect(pane.locator(".xterm")).toBeVisible();
      const header = pane.locator(".agent-header");
      await expect(header.getByText("bench", { exact: true }).first()).toBeInViewport({ ratio: 1 });
      for (const name of ["Close agent", "Back to dashboard", "Previous agent", "Next agent"]) await expect(pane.getByRole("button", { name, exact: true })).toBeInViewport({ ratio: 1 });
      await expect(pane.getByTestId("agent-pane-next-needs-you")).toBeInViewport({ ratio: 1 });
      await expect(cardTabOf(pane)).toBeInViewport({ ratio: 1 });
      await expectNoSidewaysScroll(page);

      const xterm = pane.locator(".xterm");
      const stack = pane.locator(".agent-terminal-stack");
      const before = await box(stack);
      const size = await watchResizes(pane);
      expect(size.cols, "the terminal has a size").toBeGreaterThan(0);

      await cardTabOf(pane).click();
      const panel = pane.getByTestId("agent-card-panel");
      await expect(panel).toHaveAttribute("data-card-key", await keyOf(page, "bench"));
      await expect(xterm).toBeHidden();
      const tabBox = await box(pane.locator(".agent-card-tab"));
      const panelArea = await box(pane.locator(".agent-panel"));
      for (const side of ["x", "y", "width", "height"] as const) expect(Math.abs(tabBox[side] - panelArea[side]), `the Card tab fills the terminal's place (${side})`).toBeLessThanOrEqual(TOLERANCE);
      expect(await pane.locator(".agent-card-tab").evaluate((node) => getComputedStyle(node).overflowY)).toBe("auto");
      const covered = await stack.evaluate((node) => node.getBoundingClientRect().toJSON() as { x: number; y: number; width: number; height: number });
      for (const side of ["x", "y", "width", "height"] as const) expect(Math.abs(covered[side] - before[side]), `the terminal's ${side} under the Card tab`).toBeLessThanOrEqual(TOLERANCE);
      await expectNoSidewaysScroll(page);

      // The card changes while the Card tab is shown: nothing switches, and the tab gains its bell.
      await pushCards(page, `
        const bench = byName("bench");
        bench.needs_you = true;
        bench.needs_you_reasons = ["question"];
        bench.report = { source: "agent", status: "waiting", summary: "TAB-UPDATE-2491", published_at: "2026-11-26T00:00:00Z", revision: 9 };
      `);
      await expect(panel).toContainText("TAB-UPDATE-2491");
      await expect(shownTabOf(pane)).toHaveAccessibleName("Card, needs you");
      await expect(cardTabOf(pane).getByTestId("card-tab-bell")).toBeVisible();

      await pane.getByRole("tab", { name: "Terminal" }).click();
      await expect(panel).toHaveCount(0);
      await expect(xterm).toBeVisible();
      const after = await box(stack);
      for (const side of ["x", "y", "width", "height"] as const) expect(Math.abs(after[side] - before[side]), `the terminal's ${side} after switching back`).toBeLessThanOrEqual(TOLERANCE);
      const now = await pane.evaluate((root) => {
        const recording = window as Window & RecordingWindow;
        const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
        return { cols: terminal?.cols ?? 0, rows: terminal?.rows ?? 0, resized: recording.__dadE2eResized ?? [] };
      });
      expect(now.resized, "the agent's terminal was never resized").toEqual([]);
      expect({ cols: now.cols, rows: now.rows }).toEqual(size);
      await expectNoSidewaysScroll(page);
    });
  });
}

for (const width of [375, 1440] as const) {
  test.describe(`desktop/agent-cards/105 — a disconnect keeps the shown Card tab at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    /// Scenario: At this width, open bench's pane from its card and switch to its Card tab, then let bench's daemon stop answering and answer again. While it does not answer, the pane stays on its Card tab, which says the card is not available right now, with none of its actions. When the daemon answers again, the Card tab shows bench's card, and the terminal under it has the same width it had.
    test("the Card tab stays through a disconnect and a reconnect, and the terminal keeps its width", async ({ page }) => {
      await openTree(page);
      const bench = await keyOf(page, "bench");
      await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
      const pane = page.getByTestId("agent-pane-overlay");
      const terminal = pane.locator(".agent-panel");
      const t = await box(terminal);
      await cardTabOf(pane).click();
      const panel = pane.getByTestId("agent-card-panel");
      await expect(panel).toHaveAttribute("data-card-key", bench);
      const connected = (online: boolean) => page.evaluate((value) => (window as unknown as { __dadFixtureCards: { connected(online: boolean): void } }).__dadFixtureCards.connected(value), online);

      await connected(false);
      await expect(panel.getByTestId("agent-card-gone")).toHaveText("This card is not available right now.");
      await expect(panel.getByTestId("agent-card-action-icon")).toHaveCount(0);
      await expect(shownTabOf(pane)).toHaveAccessibleName(/^Card/);

      await connected(true);
      await expect(panel).toHaveAttribute("data-card-key", bench);
      await expect(panel.getByTestId("agent-card-gone")).toHaveCount(0);
      const now = await box(terminal);
      for (const side of ["x", "width"] as const) expect(Math.abs(now[side] - t[side]), `the terminal's ${side}`).toBeLessThanOrEqual(TOLERANCE);
      await expectNoSidewaysScroll(page);
    });
  });
}

test.describe("desktop/agent-cards/104 — a kept card's pane through its daemon reconnecting", () => {
  test.use({ viewport: { width: 1440, height: 900 } });
  const connected = (page: Page, online: boolean) => page.evaluate((value) => (window as unknown as { __dadFixtureCards: { connected(online: boolean): void } }).__dadFixtureCards.connected(value), online);
  /** bench's card kept, stopped, for an agent the deck no longer lists; its read-only pane opened from the card, on its Card tab. */
  async function openEnded(page: Page): Promise<{ pane: Locator; key: string }> {
    await openTree(page);
    await pushCards(page, `const bench = byName("bench"); bench.agent_id = "bench-ended-104"; bench.status = "stopped";`);
    const key = await keyOf(page, "bench");
    await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
    const pane = page.getByTestId("ended-card-pane");
    await expect(pane).toBeVisible();
    await expect(shownTabOf(pane)).toHaveAccessibleName(/^Card/);
    await expect(pane.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", key);
    return { pane, key };
  }

  /// Scenario: Open the read-only pane of bench's card, kept for an agent the deck no longer lists: it opens on its Card tab, and its Terminal tab says bench has ended and its terminal is gone. Its daemon stops answering: the pane stays and says the card is not available right now. The daemon answers again, still keeping the card: the pane stays and shows the card again.
  test("the pane opens on Card and stays when its daemon answers again still keeping the card (review L1)", async ({ page }) => {
    const { pane, key } = await openEnded(page);
    await pane.getByRole("tab", { name: "Terminal" }).click();
    await expect(pane.getByTestId("terminal-ended")).toHaveText("bench has ended, and its terminal is gone.");
    await expect(pane.locator(".xterm")).toHaveCount(0);
    await cardTabOf(pane).click();
    await connected(page, false);
    await expect(pane.getByTestId("agent-card-gone")).toHaveText("This card is not available right now.");
    await connected(page, true);
    await expect(pane.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", key);
    await expect(pane.getByTestId("agent-card-gone")).toHaveCount(0);
    await expect(page.getByTestId("ended-card-pane")).toBeVisible();
  });

  /// Scenario: Open the same read-only pane, let its daemon stop answering, and drop bench's card while it does. When the daemon answers again and lists its cards without bench's, the pane closes and the dashboard is back.
  test("the pane closes when its daemon answers again without the card", async ({ page }) => {
    const { pane } = await openEnded(page);
    await connected(page, false);
    await expect(pane.getByTestId("agent-card-gone")).toBeVisible();
    await page.evaluate(() => {
      const hook = (window as unknown as { __dadFixtureCards: { snapshot(): { revision: number; entries: { name: string }[] }; push(next: unknown): void } }).__dadFixtureCards;
      const next = hook.snapshot();
      next.entries = next.entries.filter((entry) => entry.name !== "bench");
      next.revision += 1;
      hook.push(next);
    });
    await connected(page, true);
    await expect(page.getByTestId("ended-card-pane")).toHaveCount(0);
    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
  });
});

test.describe("desktop/agent-cards/098 — the pane's header moves between agents", () => {
  test.use({ viewport: { width: 1440, height: 900 } });

  /// Scenario: Open bench's pane from its card and switch to its Card tab. Next agent shows another agent's pane, on Terminal; Previous agent comes back to bench, on Terminal; Next needing you lands on an agent whose card needs you, on Terminal, its Card tab carrying the bell. With the keyboard on the header, `]` and `[` do the same; with the keyboard in the terminal, typing `]`, `[` and `n` reaches the agent and moves nothing.
  test("previous, next and Next needing you, by button and by key, each opening on Terminal, and the keys stay the agent's in its terminal", async ({ page }) => {
    await captureTerminals(page);
    await openTree(page);
    await pushCards(page, `byName("docs-site").needs_you = true; byName("docs-site").needs_you_reasons = ["question"];`);
    await (await cardNamed(page, "bench")).getByTestId("agent-card-details").click();
    const pane = page.getByTestId("agent-pane-overlay");
    const named = () => pane.locator(".agent-header h2").first().textContent();
    await expect(pane.locator(".agent-header h2").first()).toHaveText("bench");
    await cardTabOf(pane).click();
    await pane.getByRole("button", { name: "Next agent" }).click();
    await expect.poll(named).not.toBe("bench");
    await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
    const next = await named();
    await cardTabOf(pane).click();
    await pane.getByRole("button", { name: "Previous agent" }).click();
    await expect.poll(named).toBe("bench");
    await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
    await cardTabOf(pane).click();
    await pane.getByTestId("agent-pane-next-needs-you").click();
    await expect.poll(named).not.toBe("bench");
    await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
    await expect(cardTabOf(pane)).toHaveAccessibleName("Card, needs you");
    await expect(cardTabOf(pane).getByTestId("card-tab-bell")).toBeVisible();

    // The keys, with the keyboard on the header.
    await pane.getByRole("button", { name: "Previous agent" }).click();
    await pane.getByRole("button", { name: "Next agent" }).focus();
    await page.keyboard.press("[");
    const twoBack = await named();
    await page.keyboard.press("]");
    await expect.poll(named).not.toBe(twoBack);
    await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
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
    await expect(opened.locator(".agent-header h2").first()).toHaveText("bench");
    expect(await page.evaluate(() => (window as Window & RecordingWindow).__dadE2eSent?.join(""))).toBe("][n");
    expect(next).not.toBe("bench");
  });
});

test.describe("desktop/agent-cards/099 — the Table view's row menu opens the agent's pane", () => {
  for (const width of [375, 1440] as const) {
    /// Scenario: At this width, in the Table view, open bench's row menu and press Show card: bench's pane opens on its Terminal tab, and its Card tab shows bench's card. Back on the dashboard, its menu's most needed action — an action that asks first — opens bench's pane on Terminal under the action's review, showing its exact text, and nothing is sent until Send.
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
      await expect(shownTabOf(pane)).toHaveAccessibleName("Terminal");
      await cardTabOf(pane).click();
      await expect(pane.getByTestId("agent-card-panel")).toHaveAttribute("data-card-key", await keyOf(page, "bench"));
      await pane.getByRole("button", { name: "Back to dashboard" }).click();
      await expect(pane).toHaveCount(0);

      await row.getByTestId("overview-row-menu").click();
      await row.getByTestId("overview-row-primary").click();
      const review = page.getByTestId("agent-card-confirm");
      await expect(review).toBeVisible();
      await expect(review.getByTestId("agent-card-confirm-prompt")).toHaveText("TABLE-2491: ship it.");
      await expect(shownTabOf(page.getByTestId("agent-pane-overlay"))).toHaveAccessibleName("Terminal");
      await expect(page.getByTestId("agent-card-action-result")).toHaveCount(0);
      await review.getByTestId("agent-card-confirm-send").click();
      await expect(review.getByTestId("agent-card-action-result")).toBeVisible();
      await expectNoSidewaysScroll(page);
    });
  }
});

test.describe("desktop/agent-cards/100 — an action icon shows the pointer and the keyboard, and its tooltip sits by it", () => {
  for (const width of [375, 1440] as const) {
    /// Scenario: At this width, in the light and the dark theme, hover and keyboard-focus an action icon on a card, in the selection bar and in the Card tab. Under the pointer the icon is drawn differently than at rest (a filled, outlined button); under the keyboard it has a ring; and its tooltip sits just below or above the icon in each place.
    test(`hover marks and focus rings on the card, the bar and the Card tab at ${width}px`, async ({ page }) => {
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
        // The keyboard was used last (a Tab, which both engines count, unlike a bare modifier), so focus shows the keyboard's ring.
        await page.keyboard.press("Tab");
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
        await page.getByTestId("agent-pane-overlay").getByRole("tab", { name: /^Card/ }).click();
        const panel = page.getByTestId("agent-card-panel");
        await check(panel.getByTestId("agent-card-action-icon").first(), `${colorScheme} panel`);
        await page.getByTestId("agent-pane-overlay").getByRole("button", { name: "Back to dashboard" }).click();
        await card.getByTestId("agent-card-select").uncheck();
      }
      await expectNoSidewaysScroll(page);
    });
  }
});
