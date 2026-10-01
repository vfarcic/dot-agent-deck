import { expect, type Page } from "@playwright/test";

import { desktopScenario } from "./support";

/**
 * The desktop half of the docs screenshots (issue #1322): the production web
 * build in fixture mode — no daemon, no agent, no credential. Each scenario
 * name must also be in `xtask/screenshots/src/scenarios.rs`.
 */

/** Load a fixture state and open the agent dashboard through its rail control. */
async function overview(page: Page, state: "docs" | "docs-fleet" | "empty"): Promise<void> {
  await page.goto(`/?fixture=1&state=${state}`);
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

/** Select the fixture project's directory in the shared New agent form. */
async function projectInNewAgent(page: Page): Promise<void> {
  await overview(page, "docs");
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

desktopScenario("agent-pane", async (page) => {
  await overview(page, "docs");
  await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await expect(page.locator(".xterm-screen canvas").first()).toBeVisible();
});

desktopScenario("settings-daemons", async (page) => {
  await page.addInitScript(() => {
    window.localStorage.setItem("dot-agent-deck.desktop-settings", JSON.stringify({
      version: 1,
      appearance: { mode: "dark" },
      zoom: { level: 1 },
      // Issue #1426: a named deck, so the chooser shows the name with the
      // address beside it and the Deck name field is filled in.
      endpoints: { remote: [{ host: "build-box", id: "deck0000000000aa", name: "build", port: 22 }], selection: "deck0000000000aa" },
    }));
  });
  await page.goto("/?fixture=1&state=docs");
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-decks").click();
  await expect(page.getByTestId("settings-panel-decks")).toBeVisible();
  await expect(page.getByTestId("deck-choice-deck0000000000aa")).toContainText("build-box");
  await expect(page.getByLabel("Deck name")).toHaveValue("build");
});

desktopScenario("settings-voice", async (page) => {
  await page.goto("/?fixture=1&state=docs");
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-voice").click();
  await expect(page.getByTestId("settings-panel-voice")).toBeVisible();
});

// PRD #1260 — typing mode, desktop-only (the TUI has no voice). The fixture's
// scripted microphone says "type on" once the Voice button is pressed with the
// Desktop implementation agent's pane open; the image shows the mode marked on
// the pane's top edge and in the voice row, with Stop typing beside the button.
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
  await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
  await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  await expect(page.locator(".xterm-screen canvas").first()).toBeVisible();
  await page.getByTestId("voice-trigger").click();
  await expect(page.getByTestId("agent-pane-dictating")).toHaveText("Typing to Desktop implementation");
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
