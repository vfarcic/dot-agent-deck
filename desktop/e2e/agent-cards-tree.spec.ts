import { expect, test, type Page } from "@playwright/test";

/**
 * Issue #2418 — the Cards tree's layout, "Row, wrapping, roots stay put", in a
 * real browser at five widths.
 *
 * The fixture scene `/?fixture=1&state=docs&cards=tree` serves eight root
 * agents as a daemon that builds the card tree sends them: a dispatcher with
 * an orchestration under it, one with seven units side by side, orchestrators
 * with roles and plain agents. What is asserted is geometry, read in one
 * round trip per measurement, relative to the tree's own frame so a scroll the
 * click caused does not move anything:
 *
 * - the page never scrolls sideways, with every tree closed or every root open;
 * - opening a root moves no root sideways and resizes none; the roots in its
 *   row and the rows above do not move at all, and the rows below move down
 *   together, by one amount; closing it puts every root back;
 * - two roots opened in one row each get their own level under it, in the
 *   roots' order;
 * - the later root's line goes around the earlier tree, never through it, and
 *   every line and tree stays inside the frame;
 * - a root whose only child has a level wider than the frame under it
 *   (`cards=tree-nested`) has its line end on that child, before and after the
 *   tree's block is scrolled;
 * - a live update — a status word and the needs-you marker changing, checked
 *   references appearing — moves and resizes no root, with a tree open.
 */

const WIDTHS = [375, 768, 1024, 1440, 1920] as const;
const TOLERANCE = 0.5;

async function openCardsTree(page: Page, scene: "tree" | "tree-nested" = "tree"): Promise<void> {
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
      window.localStorage.removeItem("dot-agent-deck.cards.open");
    } catch {
      // A blocked storage leaves the default view, which the visibility check below then reports.
    }
  });
  await page.goto(`/?fixture=1&state=docs&cards=${scene}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("agent-cards-tree")).toBeVisible();
  await expect(page.locator("article[data-card-key][data-depth='0']")).toHaveCount(scene === "tree" ? 8 : 2);
}

/** A card's key in the fixture scene, by its name. */
async function keyOf(page: Page, name: string): Promise<string> {
  const key = await page.locator("article[data-card-key]", { has: page.locator(".agent-card-name", { hasText: new RegExp(`^${name}$`) }) }).first().getAttribute("data-card-key");
  if (!key) throw new Error(`no card named ${name}`);
  return key;
}

/** Change the scene's cards as a daemon's next list would: `change` edits the entries by name. */
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
    new Function("byName", body)(byName);
    next.revision += 1;
    hook.push(next);
  }, change);
}

/** The last point of a path drawn with M, V and H. */
function endOf(d: string): { x: number; y: number } {
  const all = segments(d);
  const last = all[all.length - 1];
  return { x: last.x2, y: last.y2 };
}

type Box = { key: string; row: number; x: number; y: number; w: number; h: number };

/** Every root's box, relative to the tree's frame, with the index of the row it sits in. */
async function rootBoxes(page: Page): Promise<Box[]> {
  return page.evaluate(() => {
    const frame = document.querySelector("[data-testid='agent-cards-tree']")!.getBoundingClientRect();
    const rows = [...document.querySelectorAll(".agent-cards-row")];
    return [...document.querySelectorAll<HTMLElement>("article[data-card-key][data-depth='0']")].map((node) => {
      const box = node.getBoundingClientRect();
      return { key: node.dataset.cardKey ?? "", row: rows.indexOf(node.closest(".agent-cards-row")!), x: box.x - frame.x, y: box.y - frame.y, w: box.width, h: box.height };
    });
  });
}

async function pageMetrics(page: Page) {
  return page.evaluate(() => ({
    rootScroll: document.documentElement.scrollWidth,
    rootClient: document.documentElement.clientWidth,
    bodyScroll: document.body.scrollWidth,
    bodyClient: document.body.clientWidth,
  }));
}

async function expectNoSidewaysScroll(page: Page): Promise<void> {
  const metrics = await pageMetrics(page);
  expect(metrics.bodyScroll, "content extends past the body").toBeLessThanOrEqual(metrics.bodyClient);
  expect(metrics.rootScroll, "the page scrolls sideways").toBeLessThanOrEqual(metrics.rootClient);
}

const toggleOf = (page: Page, key: string) => page.locator(`article[data-card-key="${key}"] [data-testid="agent-card-toggle"]`);

/** Each row's lines and trees, relative to the row, and the row's width. */
async function rowGeometry(page: Page) {
  return page.evaluate(() => [...document.querySelectorAll<HTMLElement>(".agent-cards-row")].map((row) => {
    const at = row.getBoundingClientRect();
    const rel = (box: DOMRect) => ({ left: box.left - at.left, right: box.right - at.left, top: box.top - at.top, bottom: box.bottom - at.top });
    return {
      width: row.clientWidth,
      bands: [...row.querySelectorAll<HTMLElement>(":scope > .agent-cards-band")].map((band) => ({
        key: band.dataset.rootKey ?? "",
        band: rel(band.getBoundingClientRect()),
        block: rel(band.querySelector(".agent-cards-block")!.getBoundingClientRect()),
      })),
      lines: [...row.querySelectorAll<SVGPathElement>(":scope > svg path")].map((path) => ({ key: path.dataset.rootKey ?? "", d: path.getAttribute("d") ?? "" })),
    };
  }));
}

/** The straight segments of a path drawn only with M, V and H, as the tree's lines are. */
function segments(d: string): { x1: number; y1: number; x2: number; y2: number }[] {
  const out: { x1: number; y1: number; x2: number; y2: number }[] = [];
  let x = 0;
  let y = 0;
  for (const [, command, value] of d.matchAll(/([MVH])\s*([-\d.]+(?:,[-\d.]+)?)/g)) {
    if (command === "M") {
      const [mx, my] = value.split(",").map(Number);
      x = mx;
      y = my;
    } else if (command === "V") {
      out.push({ x1: x, y1: y, x2: x, y2: Number(value) });
      y = Number(value);
    } else {
      out.push({ x1: x, y1: y, x2: Number(value), y2: y });
      x = Number(value);
    }
  }
  return out;
}

/** Whether a segment passes through the inside of a box. */
function crosses(segment: { x1: number; y1: number; x2: number; y2: number }, box: { left: number; right: number; top: number; bottom: number }): boolean {
  const [xa, xb] = [Math.min(segment.x1, segment.x2), Math.max(segment.x1, segment.x2)];
  const [ya, yb] = [Math.min(segment.y1, segment.y2), Math.max(segment.y1, segment.y2)];
  return xb > box.left + 1 && xa < box.right - 1 && yb > box.top + 1 && ya < box.bottom - 1;
}

for (const width of WIDTHS) {
  test.describe(`desktop/agent-cards/019 — the Cards tree at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    test("the page never scrolls sideways, with every tree closed or every root open", async ({ page }) => {
      await openCardsTree(page);
      await expectNoSidewaysScroll(page);
      const keys = (await rootBoxes(page)).map((box) => box.key);
      for (const key of keys) if (await toggleOf(page, key).count()) await toggleOf(page, key).click();
      await expect(page.getByTestId("agent-cards-band")).toHaveCount(5);
      await expectNoSidewaysScroll(page);
      for (const row of await rowGeometry(page)) {
        for (const band of row.bands) expect(band.block.right, `${band.key}'s tree stays inside the frame`).toBeLessThanOrEqual(row.width + TOLERANCE);
        for (const line of row.lines) {
          for (const segment of segments(line.d)) {
            for (const x of [segment.x1, segment.x2]) {
              expect(x, `${line.key}'s line stays inside the frame`).toBeGreaterThanOrEqual(-TOLERANCE);
              expect(x, `${line.key}'s line stays inside the frame`).toBeLessThanOrEqual(row.width + TOLERANCE);
            }
          }
        }
      }
    });

    test("opening any root leaves every root where it was; only the rows below move down", async ({ page }) => {
      await openCardsTree(page);
      const before = await rootBoxes(page);
      for (const opened of before) {
        if (!(await toggleOf(page, opened.key).count())) continue;
        await toggleOf(page, opened.key).click();
        await expect(page.locator(`[data-testid="agent-cards-band"][data-root-key="${opened.key}"]`)).toBeVisible();
        const after = await rootBoxes(page);
        const shifts = new Set<number>();
        for (const [at, was] of before.entries()) {
          const now = after[at];
          expect(now.key).toBe(was.key);
          expect(Math.abs(now.x - was.x), `${was.key} moved sideways when ${opened.key} opened`).toBeLessThanOrEqual(TOLERANCE);
          expect(Math.abs(now.w - was.w), `${was.key} changed width when ${opened.key} opened`).toBeLessThanOrEqual(TOLERANCE);
          expect(Math.abs(now.h - was.h), `${was.key} changed height when ${opened.key} opened`).toBeLessThanOrEqual(TOLERANCE);
          if (was.row <= opened.row) expect(Math.abs(now.y - was.y), `${was.key} moved when ${opened.key} opened`).toBeLessThanOrEqual(TOLERANCE);
          else {
            expect(now.y, `${was.key} moved up when ${opened.key} opened`).toBeGreaterThan(was.y);
            shifts.add(Math.round(now.y - was.y));
          }
        }
        expect(shifts.size, `the rows below ${opened.key} moved by one amount`).toBeLessThanOrEqual(1);
        await toggleOf(page, opened.key).click();
        await expect(page.getByTestId("agent-cards-band")).toHaveCount(0);
        const closed = await rootBoxes(page);
        for (const [at, was] of before.entries()) {
          expect(Math.abs(closed[at].y - was.y), `${was.key} is back when ${opened.key} closed`).toBeLessThanOrEqual(TOLERANCE);
          expect(Math.abs(closed[at].x - was.x)).toBeLessThanOrEqual(TOLERANCE);
        }
      }
    });

    test("two roots opened in one row each get their own level, and the later line goes around the earlier tree", async ({ page }) => {
      await openCardsTree(page);
      const roots = await rootBoxes(page);
      const withTree: Box[] = [];
      for (const box of roots) if (await toggleOf(page, box.key).count()) withTree.push(box);
      // The first row holding two roots with trees; at 375px each row holds one root.
      const row = withTree.find((box) => withTree.filter((other) => other.row === box.row).length >= 2)?.row;
      if (row === undefined) {
        expect(new Set(roots.map((box) => box.row)).size, "only a one-column frame has no row with two trees").toBe(roots.length);
        return;
      }
      const [first, second] = withTree.filter((box) => box.row === row);
      await toggleOf(page, second.key).click();
      await toggleOf(page, first.key).click();
      await expect(page.getByTestId("agent-cards-band")).toHaveCount(2);
      const geometry = (await rowGeometry(page))[row];
      // In the roots' order, whichever was opened first.
      expect(geometry.bands.map((band) => band.key)).toEqual([first.key, second.key]);
      const [earlier, later] = geometry.bands;
      expect(later.band.top, "the later root's tree is on a level of its own").toBeGreaterThanOrEqual(earlier.band.bottom - TOLERANCE);
      const line = geometry.lines.find((candidate) => candidate.key === second.key);
      expect(line, "the later root has a line").toBeDefined();
      for (const segment of segments(line!.d)) {
        expect(crosses(segment, earlier.block), `${second.key}'s line crosses ${first.key}'s tree: ${line!.d}`).toBe(false);
        for (const x of [segment.x1, segment.x2]) {
          expect(x).toBeGreaterThanOrEqual(-TOLERANCE);
          expect(x).toBeLessThanOrEqual(geometry.width + TOLERANCE);
        }
      }
      for (const band of geometry.bands) expect(band.block.right).toBeLessThanOrEqual(geometry.width + TOLERANCE);
      await expectNoSidewaysScroll(page);
    });

    test("a root whose only child has a level wider than the frame has its line end on that child, scrolled or not", async ({ page }) => {
      await openCardsTree(page, "tree-nested");
      const root = await keyOf(page, "deep-queue");
      await toggleOf(page, root).click();
      const child = await keyOf(page, "prd-2400");
      await toggleOf(page, child).click();
      await expect(page.locator("article[data-card-key][data-depth='2']")).toHaveCount(9);
      const attachment = () => page.evaluate(({ rootKey, childKey }) => {
        const path = document.querySelector<SVGPathElement>(`[data-testid='agent-cards-line'][data-root-key="${rootKey}"]`)!;
        const row = path.closest(".agent-cards-row")!.getBoundingClientRect();
        const card = document.querySelector(`article[data-card-key="${childKey}"]`)!.getBoundingClientRect();
        const block = document.querySelector(".agent-cards-block")!;
        return {
          d: path.getAttribute("d") ?? "",
          width: (path.closest(".agent-cards-row") as HTMLElement).clientWidth,
          card: { left: card.left - row.left, right: card.right - row.left, top: card.top - row.top },
          overflow: block.scrollWidth - block.clientWidth,
        };
      }, { rootKey: root, childKey: child });
      const expectOnChild = (at: Awaited<ReturnType<typeof attachment>>, when: string) => {
        const end = endOf(at.d);
        expect(end.x, `${when}: the line ends over the child (${at.d}; child ${at.card.left}..${at.card.right})`).toBeGreaterThanOrEqual(at.card.left - TOLERANCE);
        expect(end.x, `${when}: the line ends over the child (${at.d}; child ${at.card.left}..${at.card.right})`).toBeLessThanOrEqual(at.card.right + TOLERANCE);
        expect(Math.abs(end.y - at.card.top), `${when}: the line ends on the child's top edge (${at.d}; child top ${at.card.top})`).toBeLessThanOrEqual(1);
        for (const segment of segments(at.d)) for (const x of [segment.x1, segment.x2]) {
          expect(x).toBeGreaterThanOrEqual(-TOLERANCE);
          expect(x).toBeLessThanOrEqual(at.width + TOLERANCE);
        }
      };
      const first = await attachment();
      expect(first.overflow, "the grandchildren's level is wider than the frame").toBeGreaterThan(0);
      expectOnChild(first, "as opened");
      await page.locator(".agent-cards-block").evaluate((block) => { block.scrollLeft += 40; });
      await expect.poll(async () => endOf((await attachment()).d).x).not.toBe(endOf(first.d).x);
      expectOnChild(await attachment(), "after a scroll");
      await expectNoSidewaysScroll(page);
    });

    test("a live status change and references appearing move no root, with a tree open", async ({ page }) => {
      await openCardsTree(page);
      const triage = await keyOf(page, "triage");
      await toggleOf(page, triage).click();
      await expect(page.locator(`[data-testid="agent-cards-band"][data-root-key="${triage}"]`)).toBeVisible();
      const before = await rootBoxes(page);
      const expectUnmoved = async (what: string) => {
        const after = await rootBoxes(page);
        for (const [at, was] of before.entries()) {
          const now = after[at];
          expect(now.key).toBe(was.key);
          for (const side of ["x", "y", "w", "h"] as const) expect(Math.abs(now[side] - was[side]), `${was.key}'s ${side} changed when ${what}`).toBeLessThanOrEqual(TOLERANCE);
        }
      };

      // A root and a child under the open tree start needing the user, with longer status words.
      await pushCards(page, `
        Object.assign(byName("issue-queue"), { status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"] });
        Object.assign(byName("bench"), { status: "compacting" });
        Object.assign(byName("issue-2320"), { status: "waiting_for_input", needs_you: true, needs_you_reasons: ["native_question"] });
      `);
      await expect(page.locator(`article[data-card-key="${await keyOf(page, "issue-queue")}"] [data-testid="agent-card-needs-you"]`)).toBeVisible();
      await expectUnmoved("statuses changed");

      // Checked references appear, more than fit on one line, on a root and on a card under the open tree.
      await pushCards(page, `
        const checked = (n) => ({ field: "pull-request", kind: "pull_request", claim: "#" + n, repo: "vfarcic/dot-agent-deck", number: n, url: "https://github.com/vfarcic/dot-agent-deck/pull/" + n,
          result: { status: "checked", state: "merged", ci: { state: "failing", passing: 3, failing: 1, pending: 0, skipped: 0 }, review: "approved", title: "A pull request", checked_at: "2026-11-20T10:00:00Z", stale: false } });
        const refs = [checked(24180), checked(24181),
          { field: "issue", kind: "issue", claim: "o/r#3", repo: "o/r", number: 3, url: "https://github.com/o/r/issues/3", result: { status: "checked", state: "closed", checked_at: "2026-11-20T10:00:00Z", stale: false } },
          { field: "pr-two", kind: "pull_request", claim: "see the notes", result: { status: "unavailable", reason: "unrecognised" } },
          { field: "pr-three", kind: "pull_request", claim: "#7", number: 7, result: { status: "pending" } }];
        byName("bench").checked_refs = refs;
        byName("issue-2321").checked_refs = refs;
      `);
      await expect(page.locator(`article[data-card-key="${await keyOf(page, "bench")}"] [data-testid="agent-card-ref"]`)).toHaveCount(5);
      await expectUnmoved("references appeared");
      await expectNoSidewaysScroll(page);
    });
  });
}
