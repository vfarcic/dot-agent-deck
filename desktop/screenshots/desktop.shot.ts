import { expect, type Locator, type Page } from "@playwright/test";

import { desktopScenario } from "./support";

/**
 * The desktop half of the docs screenshots (issue #1322): the production web
 * build in fixture mode — no daemon, no agent, no credential. Each scenario
 * name must also be in `xtask/screenshots/src/scenarios.rs`.
 */

/** Load a fixture state and open the agent dashboard through its rail control. */
async function overview(page: Page, state: "docs" | "docs-fleet" | "docs-pr" | "docs-generate" | "docs-restart" | "empty", query = ""): Promise<void> {
  await page.goto(`/?fixture=1&state=${state}${query}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
}

// The same four agents, in the same states, the TUI `dashboard` image shows —
// the fixture's `docs` state, never the shared `connected` one.
desktopScenario("dashboard", async (page) => {
  await overview(page, "docs");
  await expect(page.locator(".overview-row")).toHaveCount(4);
});

desktopScenario("dashboard-empty", async (page) => {
  await overview(page, "empty");
  await expect(page.getByTestId("overview-first-run")).toBeVisible();
});

// The same synthetic permission stop as the TUI capture: one Antigravity
// agent, with its own wording beneath the tile's Needs Input status.
// Use xterm's supported DOM renderer: the sandbox's headless WebGL canvas
// stays blank, or is drawn at the wrong scale, even when its buffer holds the
// agent's output.
async function useDomTerminal(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const getContext = HTMLCanvasElement.prototype.getContext;
    Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
      value(...args: unknown[]) {
        if (typeof args[0] === "string" && args[0].includes("webgl")) return null;
        return Reflect.apply(getContext, this, args);
      },
    });
  });
}

desktopScenario("antigravity-needs-input", async (page) => {
  await useDomTerminal(page);
  await page.goto("/?fixture=1&state=docs-antigravity");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByRole("row", { name: "Open probe agent" }).click({ position: { x: 4, y: 4 } });
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  const tile = page.getByTestId("agent-tile-antigravity");
  await expect(tile).toBeVisible();
  await expect(tile.getByTestId("status-reason-1")).toHaveText("Approve Bash: rm -f probe.txt");
  await expect(tile.getByText("needs input", { exact: true })).toBeVisible();
  await expect(tile.getByTestId("question-badge-1")).toBeVisible();
  await expect(tile.getByTestId("question-badge-1")).toContainText("QUESTION");
  await expect(tile.locator(".xterm-rows")).toContainText("Esc to cancel");
});

desktopScenario("pull-request-badge", async (page) => {
  await overview(page, "docs-pr");
  await expect(page.locator(".overview-row")).toHaveCount(1);
  const badge = page.getByTestId("overview-pr-badge");
  await expect(badge).toHaveAccessibleName("Pull request #1234: open, review required; CI: 24 passing");
  await expect(badge).toHaveAttribute("data-pr-state", "open");
  await expect(badge).toHaveAttribute("data-pr-checks", "passing");
  await expect(badge).toHaveAttribute("data-pr-review", "review_required");
});

desktopScenario("dashboard-fleet", async (page) => {
  await overview(page, "docs-fleet");
  await page.getByTestId("deck-selector-toggle").click();
  await page.getByTestId("deck-selector-option-all").click();
  await expect(page.getByTestId("deck-selector-current")).toHaveText("All daemons");
  await expect(page.getByTestId("daemon-group")).toHaveCount(2);
  await expect(page.getByTestId("overview-count-decks")).toContainText("2/2");
  await expect(page.locator(".overview-row")).toHaveCount(6);
  await expect(page.getByText("API implementation", { exact: true })).toBeVisible();
});

// Issue #1496 — desktop-only: the TUI's dashboard has no filter. The same two
// daemons as `dashboard-fleet`, filtered to Working through the Filter menu.
desktopScenario("dashboard-filter", async (page) => {
  await overview(page, "docs-fleet");
  await page.getByTestId("deck-selector-toggle").click();
  await page.getByTestId("deck-selector-option-all").click();
  await expect(page.getByTestId("daemon-group")).toHaveCount(2);
  await page.getByTestId("dashboard-filter-toggle").click();
  await page.getByTestId("dashboard-filter-statuses-working").check();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("dashboard-filter-menu")).toHaveCount(0);
  await expect(page.getByTestId("dashboard-filter-line")).toContainText("Working");
  await expect(page.getByRole("button", { name: "Show all" })).toBeVisible();
});

// Issue #2399 — desktop-only: the TUI has no cards view. The `agent-cards`
// scene's cards from a daemon that does batches (`docsBatchAgentCards`, served
// only with `cards=batch`), in the Cards view: every card has its checkbox.
async function openBatchCards(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=docs&cards=batch");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-mode-cards").click();
  await expect(page.locator("article[data-card-key][data-depth='0']")).toHaveCount(2);
}

// PRD #2374, issue #2418, PR #2491 review — the Cards view as a tree, the
// orchestrator's tree opened to show its two roles beside a standalone agent.
// Every card has its checkbox (none ticked, so no selection bar), and each
// card's actions are icons in their tinted group: Go ahead on the
// orchestration's three cards and Merge it on the reviewer's.
async function openCardsTree(page: Page): Promise<void> {
  await openBatchCards(page);
  await page.getByRole("button", { name: "Open Plan / architecture's tree" }).click();
  await expect(page.locator("article[data-card-key]")).toHaveCount(4);
}

desktopScenario("agent-cards", async (page) => {
  await openCardsTree(page);
  await expect(page.getByTestId("agent-card-issue-label").first()).toHaveText("enhancement");
  await expect(page.getByRole("button", { name: "Close Plan / architecture", exact: true })).toBeVisible();
  await expect(page.getByTestId("agent-card-select")).toHaveCount(4);
  for (const box of await page.getByTestId("agent-card-select").all()) await expect(box).not.toBeChecked();
  await expect(page.getByTestId("agent-cards-selection-bar")).toHaveCount(0);
  // Each card's prompt actions are icons in its bottom row, inside the group, drawn whole.
  const groups = page.locator("article[data-card-key] [data-testid='action-icon-group']");
  await expect(groups).toHaveCount(3);
  for (const group of await groups.all()) await expect(group).toBeInViewport({ ratio: 1 });
  await expect(page.locator("[data-testid='agent-card-action-icon'][data-action='go-ahead']")).toHaveCount(3);
  await expect(page.locator("[data-testid='agent-card-action-icon'][data-action='merge']")).toHaveCount(1);
});

// A card's icon hovered: its tooltip names the action and quotes the exact text it sends.
desktopScenario("agent-cards-action-hover", async (page) => {
  await openCardsTree(page);
  const merge = page.locator("[data-testid='agent-card-action-icon'][data-action='merge']");
  await merge.hover();
  // Focus too, the keyboard's way: the capture's own options end a hover, and a focused icon keeps its tooltip shown and shows its focus ring (PR #2491 v9 review).
  await page.keyboard.press("Shift");
  await merge.focus();
  expect(await merge.evaluate((node) => node.matches(":focus-visible"))).toBe(true);
  const tip = page.getByTestId("action-tip");
  await expect(tip).toBeVisible();
  await expect(tip).toContainText("Merge it");
  await expect(tip).toContainText("Merge it, following this project's own merge procedure, then update your card.");
  await expect(tip).toBeInViewport({ ratio: 1 });
}, { tooltip: true });

// The closed orchestrator root (which selects it and its two roles) and the
// standalone agent ticked: the selection bar at the foot of the Dashboard
// offers Go ahead, which the three orchestration cards offer, Close, which
// every card offers, and the reviewer's Merge it.
async function selectBatchCards(page: Page): Promise<void> {
  await openBatchCards(page);
  await page.getByRole("checkbox", { name: "Select Plan / architecture" }).check();
  await page.getByRole("checkbox", { name: "Select User-path verification" }).check();
  await expect(page.getByTestId("agent-cards-selection-count")).toHaveText("4 selected");
}

// The selection, then Go ahead, which opens the batch's confirmation.
async function openCardsBatch(page: Page): Promise<void> {
  await selectBatchCards(page);
  await page.getByTestId("agent-cards-batch-action-go-ahead").click();
}

// Decision 2026-11-24: batch buttons are icons named by the action's label,
// with no text; each is drawn whole, with its icon, and its hover (`data-tip`)
// names it. PR #2491 review: they sit in the action group.
async function expectBatchIcons(scope: Locator, labels: readonly string[]): Promise<void> {
  const group = scope.getByTestId("action-icon-group");
  await expect(group).toBeVisible();
  const buttons = group.getByRole("button");
  await expect(buttons).toHaveCount(labels.length);
  for (const [index, label] of labels.entries()) {
    const button = buttons.nth(index);
    await expect(button).toHaveAccessibleName(label);
    await expect(button).toHaveText("");
    await expect(button.locator("svg")).toHaveCount(1);
    expect(await button.getAttribute("data-tip")).toContain(label);
    await expect(button).toBeInViewport({ ratio: 1 });
  }
}

// How cards are selected: each card's checkbox, two ticked, and the selection
// bar at the foot of the Dashboard with the count, a button per action the
// selected cards offer in the action group, and Clear. No dialog is open.
desktopScenario("agent-cards-select", async (page) => {
  await selectBatchCards(page);
  await expect(page.getByTestId("agent-card-select")).toHaveCount(2);
  await expect(page.getByRole("checkbox", { name: "Select Plan / architecture" })).toBeChecked();
  await expect(page.getByRole("checkbox", { name: "Select User-path verification" })).toBeChecked();
  const bar = page.getByTestId("agent-cards-selection-bar");
  await expect(bar).toBeInViewport({ ratio: 1 });
  await expectBatchIcons(bar, ["Go ahead", "Close", "Merge it"]);
  await expect(bar.getByRole("button", { name: "Clear", exact: true })).toBeVisible();
  await expect(page.getByTestId("agent-cards-batch-confirm")).toHaveCount(0);
});

// PR #2491 (the maintainer, 2026-11-25) — the selection bar's Edit card
// actions, on the two-daemon fleet scene: two cards ticked on the local
// daemon and the sliders button, set apart after the action group, under the
// pointer.
async function openFleetCards(page: Page): Promise<void> {
  await overview(page, "docs-fleet", "&cards=fleet");
  await page.getByTestId("overview-mode-cards").click();
  await expect(page.getByRole("checkbox", { name: "Select API review" })).toBeVisible();
}

desktopScenario("agent-cards-select-edit", async (page) => {
  await openFleetCards(page);
  await page.getByRole("checkbox", { name: "Select Plan / architecture" }).check();
  await page.getByRole("checkbox", { name: "Select User-path verification" }).check();
  const bar = page.getByTestId("agent-cards-selection-bar");
  const edit = bar.getByRole("button", { name: "Edit card actions" });
  await expect(edit).not.toHaveAttribute("aria-haspopup", /.*/);
  await expect(bar).toBeInViewport({ ratio: 1 });
  await edit.hover();
  expect(await edit.evaluate((node) => node.matches(":hover"))).toBe(true);
}, { hover: true });

// The same bar with a card ticked on each daemon: Edit card actions opens a
// menu naming both daemons, and the pointer stays on the button.
desktopScenario("agent-cards-select-edit-menu", async (page) => {
  await openFleetCards(page);
  await page.getByRole("checkbox", { name: "Select Plan / architecture" }).check();
  await page.getByRole("checkbox", { name: "Select API review" }).check();
  const bar = page.getByTestId("agent-cards-selection-bar");
  const edit = bar.getByRole("button", { name: "Edit card actions" });
  await expect(edit).toHaveAttribute("aria-haspopup", "menu");
  await edit.click();
  const menu = page.getByRole("menu", { name: "Edit card actions" });
  await expect(menu.getByRole("menuitem")).toHaveCount(2);
  await expect(menu).toBeInViewport({ ratio: 1 });
  await edit.hover();
}, { hover: true });

// PR #2491 review J7 — the Card panel scenes are shots of an agent at work,
// so they wait until its terminal has PAINTED its transcript: the DOM
// renderer's row holding `line` is in the window, every one of its text runs
// has a size, its text is drawn in a colour other than the terminal's
// background, and two frames have passed since. The shot is refused
// otherwise: a buffer that holds the text is not a picture that shows it.
async function expectTerminalPainted(page: Page, line: string): Promise<void> {
  const row = page.getByTestId("agent-pane-overlay").locator(".xterm-rows > div").filter({ hasText: line });
  await expect(row, `the terminal has a row reading "${line}"`).toHaveCount(1);
  await expect(row, "that row is in the window").toBeInViewport({ ratio: 1 });
  await expect.poll(() => row.evaluate((node) => {
    const terminal = node.closest(".xterm");
    const background = terminal ? getComputedStyle(terminal.querySelector(".xterm-viewport") ?? terminal).backgroundColor : "";
    const runs = [...node.querySelectorAll("span")].filter((span) => (span.textContent ?? "").trim() !== "");
    const sized = runs.length > 0 && runs.every((span) => { const box = span.getBoundingClientRect(); return box.width > 0 && box.height > 0; });
    const style = getComputedStyle(node);
    const inked = runs.some((span) => getComputedStyle(span).color !== background);
    return sized && inked && style.visibility !== "hidden" && Number(style.opacity) > 0;
  }), `the terminal painted "${line}"`).toBe(true);
  await page.evaluate(() => new Promise<void>((done) => requestAnimationFrame(() => requestAnimationFrame(() => done()))));
}

// PR #2491 decision 2026-11-26 — a card opens its agent's pane on its
// Terminal tab, with a Card tab beside it: Contract review's, opened from its
// card, whose agent asks whether to ship behind a version flag, so its Card
// tab carries the bell. The Card tab shows the deck-checked status and pull
// request, the agent's summary, why it needs you and its question with its two
// answers, and its actions as icons.
async function openContractReviewPane(page: Page): Promise<Locator> {
  await useDomTerminal(page);
  await openCardsTree(page);
  await page.locator("article[data-card-key]").filter({ hasText: "Contract review" }).getByTestId("agent-card-details").click();
  const pane = page.getByTestId("agent-pane-overlay");
  await expect(pane).toBeVisible();
  await expect(pane.getByRole("tab", { selected: true })).toHaveAccessibleName("Terminal");
  await expect(pane.getByRole("tab", { name: "Card, needs you" }).getByTestId("card-tab-bell")).toBeVisible();
  // PR #2491 review J7: the pane reads as an agent at work, its footer naming the project, not "Unavailable".
  await expect(pane.locator(".agent-footer")).toContainText("storefront");
  await expect(pane.locator(".agent-footer")).not.toContainText("Unavailable");
  return pane;
}

async function openContractReviewCardTab(page: Page): Promise<Locator> {
  const pane = await openContractReviewPane(page);
  await pane.getByRole("tab", { name: /^Card/ }).click();
  const panel = pane.getByTestId("agent-card-panel");
  await expect(panel).toBeVisible();
  await expect(panel.getByText("Ship the change behind a version flag?")).toBeVisible();
  await expect(panel.getByRole("button", { name: "Yes, behind a flag" })).toBeVisible();
  return panel;
}

desktopScenario("agent-card-tab-terminal", async (page) => {
  await openContractReviewPane(page);
  const rows = page.getByTestId("agent-pane-overlay").locator(".xterm-rows");
  await expect(rows).toContainText("Compared v2 and v3 responses");
  // Written once: the mount's replay is not repeated over a buffer that already holds it.
  expect(await rows.evaluate((node) => (node.textContent ?? "").split("Compared v2 and v3 responses").length - 1)).toBe(1);
  await expectTerminalPainted(page, "Compared v2 and v3 responses");
  await expectTerminalPainted(page, "Asked on the card: ship it behind a flag?");
});

desktopScenario("agent-card-tab", async (page) => {
  const panel = await openContractReviewCardTab(page);
  await expect(panel.getByTestId("agent-card-panel-refs")).toBeVisible();
  // The Card tab scrolls on its own: at its end, the question and the action icons are both in view.
  const scroller = page.getByTestId("agent-pane-overlay").locator(".agent-card-tab");
  await scroller.evaluate((node) => { node.scrollTop = node.scrollHeight; });
  await expect(panel.getByTestId("action-icon-group")).toBeInViewport({ ratio: 1 });
  await expect(panel.getByRole("button", { name: "Yes, behind a flag" })).toBeInViewport({ ratio: 1 });
});

// The same Card tab in a phone-wide window, scrolled to its Actions, so the
// shot shows the action icons and the Edit beside the heading.
desktopScenario("agent-card-tab-narrow", async (page) => {
  await page.setViewportSize({ width: 375, height: 812 });
  const panel = await openContractReviewCardTab(page);
  // The header keeps the agent's name and Close agent in the window.
  const pane = page.getByTestId("agent-pane-overlay");
  await expect(pane.getByText("Contract review", { exact: true })).toBeInViewport({ ratio: 1 });
  await expect(pane.getByRole("button", { name: "Close agent", exact: true })).toBeInViewport({ ratio: 1 });
  await expect(pane.getByRole("button", { name: "Back to dashboard" })).toBeInViewport({ ratio: 1 });
  await expect(pane.getByRole("tab", { name: /^Card/ })).toBeInViewport({ ratio: 1 });
  const actions = panel.getByTestId("agent-card-panel-actions");
  await actions.evaluate((node) => {
    const scroller = node.closest(".agent-card-tab") as HTMLElement;
    scroller.scrollTop += node.getBoundingClientRect().top - scroller.getBoundingClientRect().top - 8;
  });
  await expect(actions.getByRole("heading", { name: /^Actions/ })).toBeInViewport({ ratio: 1 });
  await expect(actions.getByRole("button", { name: "Edit card actions" })).toBeInViewport({ ratio: 1 });
  await expect(actions.getByTestId("action-icon-group")).toBeInViewport({ ratio: 1 });
});

// PR #2491 review L3 — tick every root of a pushed scene. The push gives the
// cards their checkboxes on React's next commit, so the helper waits for all
// of them (at least one) before it enumerates them — `all()` does not wait,
// and before that commit it finds none and ticks nothing — and confirms the
// selection bar counts every card before any action is pressed.
async function selectEveryRoot(page: Page, pushed: { roots: number; cards: number }): Promise<void> {
  expect(pushed.roots, "the scene has a root to tick").toBeGreaterThan(0);
  const boxes = page.locator("article[data-card-key][data-depth='0']").getByTestId("agent-card-select");
  await expect(boxes).toHaveCount(pushed.roots);
  for (const box of await boxes.all()) await box.check();
  await expect(page.getByTestId("agent-cards-selection-count")).toHaveText(`${pushed.cards} selected`);
}

// PR #2491 decision 2026-11-25 — a batch over many agents: every root of the
// `cards=tree` scene ticked, Go ahead offered by most agents, disabled on
// stopped ones and not offered by the rest. The skipped agents fold into one
// line counting them by reason, and the included names past the first few
// behind "+N more".
async function openManyBatch(page: Page, distinctTexts = false): Promise<Locator> {
  await page.goto("/?fixture=1&state=docs&cards=tree");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-mode-cards").click();
  const pushed = await page.evaluate((distinct) => {
    type Entry = Record<string, unknown> & { name: string; agent_id: string; actions: unknown[]; parent?: string };
    const hook = (window as unknown as { __dadFixtureCards: { snapshot(): { revision: number; batch?: boolean; entries: Entry[] }; push(next: unknown): void } }).__dadFixtureCards;
    const next = hook.snapshot();
    next.batch = true;
    /* PR #2491 audit K1: two agents worded their own Go ahead, each differently. */
    const own = distinct ? new Map([[2, "Go ahead, then also delete the scratch branch."], [7, "Go ahead and push straight to main."]]) : new Map<number, string>();
    next.entries.forEach((entry, at) => {
      if (at % 5 === 3) return;
      const stopped = at % 5 === 1;
      const text = own.get(at);
      entry.actions = [{ id: "go-ahead", label: "Go ahead", kind: "prompt", prompt: text ?? "Go ahead.", prompt_source: text ? "agent" : "catalog", text_input: false, confirm: false, confirm_detail: null, icon: "play",
        placement: "card", custom: text !== undefined, source: text ? "agent" : "catalog", enabled: !stopped, disabled_reason: stopped ? "agent_stopped" : null, last_delivery: null, token: `tok-many-${entry.agent_id}` }, ...entry.actions];
    });
    next.revision += 1;
    hook.push(next);
    return { roots: next.entries.filter((entry) => !entry.parent).length, cards: next.entries.length };
  }, distinctTexts);
  await selectEveryRoot(page, pushed);
  await page.getByTestId("agent-cards-batch-action-go-ahead").click();
  const dialog = page.getByTestId("agent-cards-batch-confirm");
  await expect(dialog).toHaveAttribute("data-stage", "review");
  await expect(dialog.getByTestId("agent-cards-batch-skipped-toggle")).toHaveText(/^\d+ skipped: (?=.*\d+ don't offer Go ahead)(?=.*\d+ aren't running)/);
  return dialog;
}

desktopScenario("agent-cards-batch-many", async (page) => {
  const dialog = await openManyBatch(page);
  await expect(dialog.getByTestId("agent-cards-batch-skipped-toggle")).toHaveAttribute("aria-expanded", "false");
  await expect(dialog.getByTestId("agent-cards-batch-skipped-group")).toHaveCount(0);
});

// PR #2491 audit K1 — the same batch with two agents that worded their own Go
// ahead: every distinct text is quoted with who wrote it, the catalog's text
// first with its long list folded behind +N more, and the heading counts the
// agents and the texts.
desktopScenario("agent-cards-batch-texts", async (page) => {
  const dialog = await openManyBatch(page, true);
  await expect(dialog.getByTestId("agent-cards-batch-prompt")).toHaveText(["Go ahead.", "Go ahead, then also delete the scratch branch.", "Go ahead and push straight to main."]);
  await expect(dialog.getByTestId("agent-cards-batch-source")).toHaveText(["catalog", "agent says", "agent says"]);
  await expect(dialog.getByRole("heading", { level: 3 }).first()).toContainText("3 different texts");
  await expect(dialog.getByTestId("agent-cards-batch-more")).toHaveText(/^\+\d+ more$/);
});

// PR #2491 audit K1 — the same confirmation in a short window, the two texts
// the agents wrote below the fold: Send waits, disabled, with the hint and
// Show next text beside it, until each text has been in view.
desktopScenario("agent-cards-batch-texts-unseen", async (page) => {
  await page.setViewportSize({ width: 1440, height: 500 });
  const dialog = await openManyBatch(page, true);
  await expect(dialog.getByTestId("agent-cards-batch-prompt")).toHaveCount(3);
  await expect(dialog.getByTestId("agent-cards-batch-prompt").nth(2)).not.toBeInViewport();
  await expect(dialog.getByRole("button", { name: "Send", exact: true })).toBeDisabled();
  await expect(dialog.getByTestId("agent-cards-batch-unseen")).toHaveText("Scroll to see all 3 texts");
  await expect(dialog.getByRole("button", { name: "Show next text" })).toBeInViewport({ ratio: 1 });
});

desktopScenario("agent-cards-batch-many-expanded", async (page) => {
  const dialog = await openManyBatch(page);
  await dialog.getByTestId("agent-cards-batch-skipped-toggle").click();
  await expect(dialog.getByTestId("agent-cards-batch-skipped-group")).toHaveCount(2);
  // The dialog's list scrolls: the skipped line at its top, so both reasons' groups are in view.
  await dialog.getByTestId("agent-cards-batch-skipped-toggle").evaluate((node) => node.scrollIntoView({ block: "start" }));
  await expect(dialog.getByTestId("agent-cards-batch-skipped-group").nth(1).getByRole("heading")).toBeInViewport();
});

desktopScenario("agent-cards-batch", async (page) => {
  await openCardsBatch(page);
  const dialog = page.getByTestId("agent-cards-batch-confirm");
  await expect(dialog.locator(".agent-cards-batch-entry.is-included")).toHaveCount(3);
  // Decision 2026-11-24: the text all three get is quoted once, above the three of them.
  await expect(dialog.getByTestId("agent-cards-batch-prompt")).toHaveText(["Go ahead."]);
  await expect(dialog.getByTestId("agent-cards-batch-group")).toHaveCount(1);
  await expect(dialog.getByTestId("agent-cards-batch-skipped-toggle")).toHaveText("1 skipped: 1 doesn't offer Go ahead");
  await expect(dialog.getByRole("button", { name: "Send", exact: true })).toBeEnabled();
});

// The same batch after Send: two agents queued, and one the daemon refused
// because it stopped after the preview, so the headline reads as a partial
// failure.
desktopScenario("agent-cards-batch-results", async (page) => {
  await openCardsBatch(page);
  await page.getByTestId("agent-cards-batch-confirm").getByRole("button", { name: "Send", exact: true }).click();
  const results = page.getByTestId("agent-cards-batch-results");
  await expect(results.getByTestId("agent-cards-batch-entry")).toHaveCount(3);
  await expect(results.locator("[data-outcome='failed']")).toContainText("Go ahead is disabled: the agent is not running");
  await expect(results.getByRole("status")).toHaveText("2 queued for the agent of 3; 1 failed");
  await expect(results.getByRole("button", { name: "Close", exact: true })).toBeVisible();
});

// PRD #2374 decision 2026-11-24 — desktop-only: the TUI has no cards view.
// The `cards=tree` scene's Cards view: its daemon advertises the Card
// actions editor (the fixture bridge answers from the deck's two defaults).
async function openTreeCards(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=docs&cards=tree");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-mode-cards").click();
  await expect(page.getByTestId("agent-cards-tree")).toBeVisible();
}

// Issue #2533 — two trees opened in one row, each in its own captioned box:
// issue-queue with its orchestration prd-2374 opened inside it, and
// prd-queue's seven agents wrapped into two rows under a bus each.
desktopScenario("agent-cards-wrapped", async (page) => {
  await page.setViewportSize({ width: 1440, height: 1180 });
  await openTreeCards(page);
  const toggleOf = (name: string) => page.locator("article[data-card-key]").filter({ has: page.getByRole("button", { name: `Open ${name}`, exact: true }) }).getByTestId("agent-card-toggle");
  for (const name of ["issue-queue", "prd-queue", "prd-2374"]) await toggleOf(name).click();
  await expect(page.getByTestId("agent-cards-box")).toHaveCount(2);
  await expect(page.getByTestId("agent-cards-box-caption")).toHaveText(["issue-queue", "prd-queue"]);
  await expect(page.locator("article[data-card-key][data-depth='2']")).toHaveCount(3);
  const prdQueue = page.locator("[data-testid='agent-cards-band']").nth(1).locator(":scope [data-testid='agent-cards-tree-level']").first();
  await expect(prdQueue).toHaveAttribute("data-rows", "2");
});

// PR #2491 decision 2026-11-25 — the daemon's ⋯ menu, just left of its New
// agent button, open on Card actions; no line of counts or buttons above
// the cards.
desktopScenario("agent-cards-daemon-menu", async (page) => {
  await openTreeCards(page);
  await expect(page.locator(".agent-cards-toolbar")).toHaveCount(0);
  await page.getByTestId("daemon-more").first().click();
  const menu = page.getByTestId("daemon-more-menu");
  await expect(menu.getByRole("menuitem", { name: "Card actions" })).toBeVisible();
  await expect(menu).toBeInViewport({ ratio: 1 });
});

// PR #2491 decision 2026-11-25 — the Dashboard's ⋯ menu, right of Refresh,
// open on Needs you only (not ticked), with the count of agents that need
// you on the ⋯.
desktopScenario("dashboard-more-menu", async (page) => {
  await openTreeCards(page);
  await expect(page.getByTestId("overview-needs-you-count")).toHaveText("3");
  await page.getByRole("button", { name: "More", exact: true }).click();
  const menu = page.getByTestId("overview-more-menu");
  await expect(menu.getByRole("menuitemcheckbox", { name: "Needs you only" })).toHaveAttribute("aria-checked", "false");
  await expect(menu).toBeInViewport({ ratio: 1 });
});

// PR #2491 decision 2026-11-25 — Needs you only on, in the Table view: the
// chip beside Filter, and only the agents that need you with the agents above
// them (issue-queue, prd-2374, tester, issue-2310, docs-site, editor).
desktopScenario("dashboard-needs-you-chip", async (page) => {
  await openTreeCards(page);
  await page.getByTestId("overview-mode-table").click();
  await page.getByRole("button", { name: "More", exact: true }).click();
  await page.getByRole("menuitemcheckbox", { name: "Needs you only" }).click();
  await expect(page.getByTestId("overview-more-menu")).toHaveCount(0);
  await expect(page.getByTestId("needs-you-chip")).toBeVisible();
  await expect(page.getByRole("row", { name: /^Open .* agent$/ })).toHaveCount(6);
  await page.mouse.move(0, 0);
});

// The Card actions editor on the `cards=tree` scene's daemon, opened from its
// ⋯ menu and titled with its name, with Add action's form filled in for a
// Deploy it action and its icon grid open on the rocket chosen.
desktopScenario("agent-cards-actions-editor", async (page) => {
  await openTreeCards(page);
  await page.getByTestId("daemon-more").first().click();
  await page.getByRole("menuitem", { name: "Card actions" }).click();
  const editor = page.getByRole("dialog", { name: /^Card actions: / });
  await expect(editor.getByRole("combobox", { name: "Daemon" })).toHaveCount(0);
  await expect(editor.locator('[data-testid="agent-card-actions-editor-row"][data-action="go-ahead"]')).toHaveAttribute("data-source", "deck");
  await expect(editor.locator('[data-testid="agent-card-actions-editor-row"][data-action="merge"]')).toHaveAttribute("data-source", "deck");
  await editor.getByRole("button", { name: "Add action", exact: true }).click();
  await editor.getByLabel("Name", { exact: true }).fill("deploy");
  await editor.getByLabel("Label", { exact: true }).fill("Deploy it");
  await editor.getByLabel("Text it sends", { exact: true }).fill("Deploy it to staging.\nThen update your card.");
  await editor.getByLabel("Tell agents when to offer it", { exact: true }).fill("Offer when the change can be deployed.");
  await editor.getByLabel("Ask before sending", { exact: true }).check();
  await editor.getByRole("button", { name: "Choose icon", exact: true }).click();
  await editor.getByRole("listbox", { name: "Icons" }).getByRole("option", { name: "rocket", exact: true }).click();
  await editor.getByRole("button", { name: "Choose icon", exact: true }).click();
  const grid = editor.getByRole("listbox", { name: "Icons" });
  await expect(grid.getByRole("option", { name: "rocket", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(editor.getByTestId("agent-card-actions-editor-icon")).toHaveAttribute("data-icon", "rocket");
  await expect(grid).toBeInViewport();
});

// PRD #1258 — the same fixed host the TUI `host-metrics` image shows
// (`HOST_SAMPLE` in `tests/e2e_docs_screenshots.rs`), on the `docs` deck.
// The host button beside the deck's name, opened: its popover is what the
// image shows, with the temp root's warning and its X (PR #1672 round 10).
desktopScenario("host-metrics", async (page) => {
  await overview(page, "docs");
  await expect(page.getByTestId("host-metrics-button")).toHaveAttribute("data-level", "warn");
  await page.getByTestId("host-metrics-button").click();
  await expect(page.getByTestId("host-metrics")).toContainText("0.42 across 16 cores");
  await expect(page.getByTestId("host-metrics")).toContainText("6 GiB free of 64 GiB total (warning)");
  await expect(page.getByTestId("host-metrics")).toContainText("Memory available43.5 GiB");
  await expect(page.getByTestId("host-metrics-popover").getByRole("button", { name: "Close host" })).toBeVisible();
});

// PRD #2119 — the Generate config confirmation the TUI `generate-config` image
// shows, opened from the Plan / architecture agent's pane (issue #2454 took the
// button off the Dashboard row): the fixture's `docs-generate` state, whose one
// agent has a pane.
desktopScenario("generate-config", async (page) => {
  await overview(page, "docs-generate");
  await expect(page.locator(".overview-row")).toHaveCount(1);
  await page.getByRole("row", { name: "Open Plan / architecture agent" }).click({ position: { x: 4, y: 4 } });
  await page.getByTestId("agent-pane-overlay").getByRole("button", { name: "Generate config", exact: true }).click();
  const dialog = page.getByRole("alertdialog", { name: "Generate .dot-agent-deck.toml" });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText("No orchestration config found for this project.");
  await expect(dialog.getByRole("button", { name: "Yes", exact: true })).toBeVisible();
  await expect(dialog.getByRole("button", { name: "No", exact: true })).toBeVisible();
});

// The two agents the TUI `restart-notice` image names, stopped by a restart of
// the fixture's `docs-restart` deck after its first listing was shown. The
// preview daemon also lists interrupted work (`interrupted=1`), so the notice
// carries its line and Show button (PRD #1790 stage 2).
desktopScenario("restart-notice", async (page) => {
  await overview(page, "docs-restart", "&interrupted=1");
  const toast = page.getByTestId("restart-toast");
  await expect(toast).toContainText("The deck restarted; these agents were stopped: api-fix, docs-review.");
  await expect(toast.getByTestId("restart-toast-interrupted")).toContainText("3 interrupted items from before the restart");
  await expect(toast.getByTestId("restart-toast-interrupted-show")).toBeVisible();
  await expect(page.locator(".overview-row")).toHaveCount(0);
});

// PRD #1790 stage 2 — the "Interrupted work" dialog over the docs dashboard,
// opened from the deck's banner: the preview daemon's three items
// (`desktop/src/data/fixtureInterrupted.ts`), the unit `fix-login` selected.
desktopScenario("interrupted-work", async (page) => {
  await page.goto("/?fixture=1&state=docs&interrupted=1");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("interrupted-banner")).toContainText("3 interrupted items from before the restart");
  await page.getByTestId("interrupted-banner-show").click();
  const dialog = page.getByTestId("interrupted-dialog");
  await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
  await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
  await expect(dialog.getByTestId("interrupted-reclaim")).toBeVisible();
  await expect(dialog.getByTestId("interrupted-other-agents")).toHaveText("Other agents");
});

/** Select the fixture project's directory in the shared New agent form. */
async function projectInNewAgent(page: Page, query = ""): Promise<void> {
  await page.goto(`/?fixture=1&state=docs${query}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-new-agent").click();
  await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/demo-project");
  await page.keyboard.press(" ");
  await expect(page.getByTestId("new-agent-dir")).toHaveText("/home/dev/demo-project");
}

desktopScenario("new-agent", async (page) => {
  await projectInNewAgent(page);
  await expect(page.getByTestId("new-agent-dialog")).toBeVisible();
  await expect(page.getByTestId("new-agent-name")).toHaveValue("demo-project");
  await page.getByTestId("new-agent-name").evaluate((input) => (input as HTMLInputElement).blur());
});

desktopScenario("orchestration", async (page) => {
  await projectInNewAgent(page);
  await page.getByTestId("new-agent-mode-orch:demo-loop").click();
  await page.getByTestId("new-agent-name").fill("demo-loop");
  await page.getByTestId("new-agent-start").click();
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByText("ORCHESTRATOR", { exact: true })).toBeVisible();
  await expect(page.locator(".overview-row")).toHaveCount(6);
});

// PRD #1885 — the same review the TUI `project-trust` image shows: `team` in
// `demo-project`, changed since it was approved (`?trust=changed`, whose roles
// are `DOCS_TRUST_ROLES` in `desktop/src/data/fixture.ts`), opened by starting
// it from the New agent dialog. A taller window than the other scenes, because
// at the type scale's sizes (issue #2312) the review of four roles scrolls in a
// 1280×800 one, and the image has to show every role, the removed one included.
desktopScenario("project-trust", async (page) => {
  await page.setViewportSize({ width: 1280, height: 1000 });
  await projectInNewAgent(page, "&trust=changed");
  await page.getByTestId("new-agent-mode-orch:team").click();
  await page.getByTestId("new-agent-name").fill("team");
  await page.getByTestId("new-agent-start").click();
  const dialog = page.getByTestId("project-trust-dialog");
  await expect(dialog).toBeVisible();
  await expect(page.getByTestId("project-trust-state")).toHaveText("changed since you approved it");
  await expect(page.getByTestId("project-trust-deck")).toBeVisible();
  await expect(page.getByTestId("project-trust-role")).toHaveCount(3);
  await expect(page.getByTestId("project-trust-removed")).toContainText("reviewer");
  await expect(page.getByTestId("project-trust-removed")).toBeInViewport();
  await expect(dialog.getByRole("button", { name: "Approve and start" })).toBeVisible();
});

// PRD #2174 — the star reminder the TUI `star-prompt` image shows, on a launch
// it shows at (`?star=1`), over the empty dashboard.
desktopScenario("star-prompt", async (page) => {
  await page.goto("/?fixture=1&state=empty&star=1");
  const dialog = page.getByRole("alertdialog", { name: "Enjoying dot-agent-deck?" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole("button", { name: "s Star" })).toBeInViewport();
});

desktopScenario("agent-pane", async (page) => {
  await overview(page, "docs");
  await page.getByRole("row", { name: "Open Desktop implementation agent" }).click({ position: { x: 4, y: 4 } });
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await expect(page.locator(".xterm-screen canvas").first()).toBeVisible();
  await expect(page.getByTestId("agent-pane-overlay").getByRole("button", { name: "Close agent", exact: true })).toBeVisible();
});

desktopScenario("agent-messaging", async (page) => {
  await page.goto("/?fixture=1&state=docs&messaging=1&experimental=1");
  await page.getByTestId("open-deck").click();
  await page.getByRole("button", { name: "Close events drawer" }).click();
  await expect(page.getByTestId("inbox-badge-1")).toBeVisible();
  await expect(page.getByTestId("inbox-badge-2")).toBeVisible();
  await expect(page.getByTestId("inbox-badge-1")).toHaveText("✉1 queued");
  await expect(page.getByTestId("inbox-badge-2")).toHaveText("✉1 queued");
  await expect(page.getByTestId("terminal-1").locator(".xterm-screen canvas").first()).toBeVisible();
  await expect(page.getByTestId("terminal-2").locator(".xterm-screen canvas").first()).toBeVisible();
});

desktopScenario("inbox-attention", async (page) => {
  await page.goto("/?fixture=1&state=docs&messaging=attention&experimental=1");
  await page.getByTestId("open-deck").click();
  await page.getByRole("button", { name: "Close events drawer" }).click();
  const row = page.getByTestId("inbox-attention-m-k3xzq2ab-0000000000000001");
  await expect(row).toBeVisible();
  await expect(row.getByRole("button", { name: "Retry" })).toBeVisible();
  await expect(row.getByRole("button", { name: "Cancel" })).toBeVisible();
});

desktopScenario("settings-daemons", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "dark" },
      zoom: { level: 1 },
      // Issue #1426: a named deck, so the chooser shows the name with the
      // address beside it and the Daemon name field is filled in.
      endpoints: { remote: [{ host: "build-box", id: "deck0000000000aa", name: "build", port: 22 }], selection: "deck0000000000aa" },
    }));
  });
  await page.goto("/?fixture=1&state=docs");
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-decks").click();
  await expect(page.getByTestId("settings-panel-decks")).toBeVisible();
  await expect(page.getByTestId("deck-choice-deck0000000000aa")).toContainText("build-box");
  await expect(page.getByLabel("Daemon name")).toHaveValue("build");
});

// PRD #2274 — desktop-only: the TUI attaches to one daemon at a time. The
// empty Dashboard of a fresh install, with only this machine configured, and
// the Add a daemon dialog its empty-agents note opens, filled in.
desktopScenario("add-daemon", async (page) => {
  await overview(page, "empty");
  await page.getByTestId("overview-first-run-add-daemon").click();
  const dialog = page.getByRole("dialog", { name: "Add a daemon" });
  await expect(dialog).toBeVisible();
  await dialog.getByLabel("Host", { exact: true }).fill("build-box");
  await dialog.getByLabel("User", { exact: true }).fill("dev");
  await expect(dialog.getByTestId("add-daemon-save")).toBeEnabled();
});

// PRD #1487 — Upgrade on a remote daemon older than the app, stopped at the
// question the daemon asks while agents are running: the fixture's `upgrade`
// fleet, whose dev@build-box deck runs an older release with agents on it.
desktopScenario("daemon-upgrade", async (page) => {
  await page.goto("/?fixture=1&state=upgrade");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  const buildBox = page.getByTestId("daemon-group").filter({ has: page.getByTestId("daemon-identity").getByText("dev@build-box", { exact: true }) });
  await buildBox.getByTestId("daemon-upgrade").click();
  await page.getByTestId("upgrade-start").click();
  await expect(page.getByTestId("upgrade-decision")).toBeVisible();
  await expect(page.getByTestId("upgrade-stage-installing")).toHaveAttribute("data-state", "done");
  await expect(page.getByTestId("upgrade-at-stake").getByRole("listitem").first()).toBeVisible();
});

// Issue #1600 — Upgrade all from the Dashboard's top bar over the `upgrade`
// fleet, stopped at dev@build-box's own question while ci@runner-7 waits.
// Nothing is installed; the fixture bridge plays the crate's batch.
desktopScenario("upgrade-all", async (page) => {
  await page.goto("/?fixture=1&state=upgrade");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-upgrade-all").click();
  await expect(page.getByTestId("upgrade-all-start")).toBeVisible();
  await page.getByTestId("upgrade-all-start").click();
  await expect(page.getByTestId("upgrade-all-decision")).toBeVisible();
  await expect(page.getByTestId("upgrade-all-at-stake").getByRole("listitem").first()).toBeVisible();
});

// Issue #1635 — upgrading the app: the `docs` dashboard behind the
// newer-release banner and the rail's upgrade button, and the dialog the
// banner opens, stopped at its question. The fixture's stand-in plays a
// `.dmg` app and a Homebrew CLI on v0.46.0 with v0.47.0 out; since issue
// #2062 the banner's dialog covers the CLI too, after the app. Nothing is
// installed.
desktopScenario("self-upgrade", async (page) => {
  await page.goto("/?fixture=1&state=docs&selfupgrade=1");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.locator(".overview-row")).toHaveCount(4);
  await expect(page.getByTestId("self-upgrade-rail")).toBeVisible();
  await page.getByTestId("self-upgrade-banner-open").click();
  await expect(page.getByTestId("self-upgrade-dialog")).toHaveAttribute("data-phase", "confirm");
  await expect(page.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "app");
  await expect(page.getByTestId("self-upgrade-plan-cli")).toBeVisible();
  await expect(page.getByTestId("self-upgrade-then")).toBeVisible();
});

// Issue #1753 — upgrading this machine's CLI from the Local daemon section:
// the app is current, so there is no banner, and the Homebrew CLI is one
// release behind; the dialog Upgrade opens, stopped at its question.
desktopScenario("local-cli-upgrade", async (page) => {
  await page.goto("/?fixture=1&state=docs&selfupgrade=cli");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.locator(".overview-row")).toHaveCount(4);
  await expect(page.getByTestId("self-upgrade-banner")).toHaveCount(0);
  await page.getByRole("button", { name: "Upgrade the CLI on this machine" }).click();
  await expect(page.getByTestId("self-upgrade-dialog")).toHaveAttribute("data-phase", "confirm");
  await expect(page.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
  await expect(page.getByTestId("self-upgrade-then")).toBeVisible();
});

desktopScenario("settings-voice", async (page) => {
  await page.goto("/?fixture=1&state=docs");
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-voice").click();
  await expect(page.getByTestId("settings-panel-voice")).toBeVisible();
});

// PRD #1497 — reading's two Settings rows, below the fold of settings-voice:
// Reading with its disclosure, and Speech source.
desktopScenario("settings-voice-reading", async (page) => {
  await page.goto("/?fixture=1&state=docs");
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-voice").click();
  await expect(page.getByTestId("settings-panel-voice")).toBeVisible();
  // The key row above the reading rows says the BROWSER PREVIEW has no
  // credential store, which the app never shows, so it is left out of the shot.
  await page.addStyleTag({ content: "[data-testid^='secret-problem-'] { display: none !important; }" });
  await page.getByTestId("voice-speech-hint").scrollIntoViewIfNeeded();
  await expect(page.getByTestId("voice-reading-disclosure")).toBeInViewport();
  await expect(page.getByTestId("voice-speech-hint")).toBeInViewport();
});

// PRD #1260 — typing mode, desktop-only (the TUI has no voice). The fixture's
// scripted microphone says "type on" once the Voice button is pressed with the
// Desktop implementation agent's pane open; the image shows the mode marked at
// the window's top edge and in the voice row, with Stop typing beside the button.
desktopScenario("voice-typing-mode", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "system" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  });
  await page.goto("/?fixture=1&state=docs&voice=type%20on");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByRole("row", { name: "Open Desktop implementation agent" }).click({ position: { x: 4, y: 4 } });
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await expect(page.locator(".xterm-screen canvas").first()).toBeVisible();
  await page.getByTestId("voice-trigger").click();
  await expect(page.getByTestId("voice-typing-indicator")).toHaveText("Typing to Desktop implementation");
  await expect(page.getByTestId("voice-dictating")).toBeVisible();
  await expect(page.getByTestId("voice-stop-typing")).toBeVisible();
});

// PRD #1261 — the numbered choice, desktop-only (the TUI has no voice). The
// fixture's scripted microphone says "open the agent" once the Voice button is
// pressed on the dashboard, which the preview answers with a canned tie between
// the two agents labelled Plan / architecture and Desktop implementation; the
// image shows the choice dialog centred over the dashboard — its numbered
// entries, countdown and Cancel — with the voice row below saying what was heard.
desktopScenario("voice-choice", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "system" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  });
  await page.goto("/?fixture=1&state=docs&voice=open%20the%20agent");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.locator(".overview-row")).toHaveCount(4);
  await page.getByTestId("voice-trigger").click();
  const choice = page.getByRole("dialog", { name: "Which agent?" });
  await expect(choice.getByRole("button", { name: "1. Plan / architecture" })).toBeVisible();
  await expect(choice.getByRole("button", { name: "2. Desktop implementation" })).toBeVisible();
  await expect(choice.getByRole("timer")).toBeVisible();
  await expect(page.getByTestId("voice-report")).toContainText("open the agent");
});

// PRD #1631 — answering an agent's question, desktop-only (the TUI has no
// voice). The `docs-question` fixture stops the Desktop implementation agent
// (Codex) on a command approval; with its pane open, the scripted microphone
// says "go ahead", and the image shows the voice row's answer, naming the
// option and the command, with its countdown and Cancel, beside the prompt in
// the pane.
desktopScenario("voice-question", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "system" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  });
  await page.goto("/?fixture=1&state=docs-question&voice=go%20ahead");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByRole("row", { name: "Open Desktop implementation agent" }).click({ position: { x: 4, y: 4 } });
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await expect(page.locator(".xterm-screen canvas").first()).toBeVisible();
  await page.getByTestId("voice-trigger").click();
  await expect(page.getByTestId("voice-question")).toContainText("“Yes, proceed” (allow once) — npm install --save-dev msw — sending in");
  await expect(page.getByTestId("voice-question-cancel")).toBeVisible();
});

// PR #1451 round 3, change 3 — numbers on lists while voice is on,
// desktop-only. The two-daemon docs fleet's dashboard with the Voice button
// pressed and nothing said: each agent row shows its number, one sequence
// across both daemons.
desktopScenario("voice-numbers", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "system" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  });
  await page.goto("/?fixture=1&state=docs-fleet");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  const rows = page.locator(".overview-row");
  await expect(rows).toHaveCount(6);
  await page.getByTestId("voice-trigger").click();
  await expect(page.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "true");
  for (let index = 0; index < 6; index += 1) await expect(rows.nth(index)).toHaveAccessibleName(new RegExp(`^${index + 1}\\.`));
});

// PR #1451 round 3, change 4 — a directory too long for the dialog, shown a
// page at a time while voice is on. The fixture's crowded `voice-pages` state:
// its microphone says nothing, so the page is the first one, as opened.
desktopScenario("voice-pages", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "system" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  });
  await page.goto("/?fixture=1&state=voice-pages");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await page.getByTestId("overview-new-agent").click();
  // The dialog's backdrop covers the voice row for the pointer; the keyboard reaches it.
  await page.getByTestId("voice-trigger").focus();
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "true");
  await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
  await expect(page.getByTestId("new-agent-directory-page")).toHaveText(/^Page 1 of [2-9]/);
  await expect(page.getByTestId("new-agent-directory-list").getByRole("option").first()).toHaveAccessibleName(/^1\./);
});
