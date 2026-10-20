import { expect, type Page } from "@playwright/test";

import { desktopScenario } from "./support";

/**
 * The desktop half of the docs screenshots (issue #1322): the production web
 * build in fixture mode — no daemon, no agent, no credential. Each scenario
 * name must also be in `xtask/screenshots/src/scenarios.rs`.
 */

/** Load a fixture state and open the agent dashboard through its rail control. */
async function overview(page: Page, state: "docs" | "docs-fleet" | "docs-pr" | "empty"): Promise<void> {
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

desktopScenario("pull-request-badge", async (page) => {
  await overview(page, "docs-pr");
  await expect(page.locator(".overview-row")).toHaveCount(1);
  const badge = page.getByTestId("overview-pr-badge");
  await expect(badge).toHaveAccessibleName("Pull request #1234: open, review required");
  await expect(badge).toHaveAttribute("data-pr-state", "open");
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

// Issue #1635 — upgrading the app: the `docs` dashboard behind the
// newer-release banner and the rail's upgrade button, and the dialog the
// banner opens, stopped at its question. The fixture's stand-in plays a
// `.dmg` app and a Homebrew CLI on v0.46.0 with v0.47.0 out; since issue
// #1753 the banner's dialog covers the app alone. Nothing is installed.
desktopScenario("self-upgrade", async (page) => {
  await page.goto("/?fixture=1&state=docs&selfupgrade=1");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.locator(".overview-row")).toHaveCount(4);
  await expect(page.getByTestId("self-upgrade-rail")).toBeVisible();
  await page.getByTestId("self-upgrade-banner-open").click();
  await expect(page.getByTestId("self-upgrade-dialog")).toHaveAttribute("data-phase", "confirm");
  await expect(page.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "app");
  await expect(page.getByTestId("self-upgrade-plan-cli")).toHaveCount(0);
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
  await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
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
  await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
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
