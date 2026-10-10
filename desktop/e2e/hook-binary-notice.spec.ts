import { expect, type Locator, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * Issue #1637 — the per-deck notice that an agent's hooks run an older
 * `dot-agent-deck`, as a user sees it.
 *
 * The `hook-notice` fixture fleet plays two daemons: this machine's, whose
 * Claude Code and Codex hooks still run an older Homebrew copy, and
 * `dev@build-box`, with nothing to report. The notice is a fact about one
 * deck, so it must appear on that deck and nowhere else.
 */

const BUILD_BOX = "dev@build-box";

async function openHookNoticeFleet(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=hook-notice");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
}

/** A deck's card on the dashboard, by the name its header shows. */
function card(page: Page, name: string): Locator {
  return page.getByTestId("daemon-group").filter({ has: page.getByTestId("daemon-identity").getByText(name, { exact: true }) });
}

test.describe("Hook binary notice", () => {
  /** Scenario: open the dashboard; the local deck's card names the stale hooks and the fix, and the remote deck's card has no notice. */
  test("is shown on the affected deck's card and not on another deck", async ({ page }) => {
    await openHookNoticeFleet(page);
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group")).toHaveCount(2);

    const local = card(page, "Local daemon").getByTestId("hook-binary-notice");
    await expect(local).toBeVisible();
    await expect(local).toContainText("Claude Code, Codex hooks run dot-agent-deck 0.45.1 (/opt/homebrew/bin/dot-agent-deck); this deck is 0.46.0");
    await expect(local.getByTestId("hook-binary-notice-remedy")).toHaveText("Run: brew upgrade dot-agent-deck");
    await expect(local.getByTestId("hook-binary-notice-command")).toHaveText("brew upgrade dot-agent-deck");
    await expect(local.getByTestId("hook-binary-notice-copy")).toBeVisible();

    await expect(card(page, BUILD_BOX).getByTestId("hook-binary-notice")).toHaveCount(0);
  });

  /** Scenario: on the affected deck's own screen the notice sits under the header and the deck's controls stay usable. */
  test("does not block the deck screen", async ({ page }) => {
    await openHookNoticeFleet(page);
    const notice = page.getByTestId("hook-binary-notice");
    await expect(notice).toHaveCount(1);
    await expect(notice).toContainText("brew upgrade dot-agent-deck");
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    // The rest of the screen still answers: the dashboard is one click away.
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group")).toHaveCount(2);
  });

  /**
   * Scenario: the real live bridge, with Tauri's IPC mocked, receives a
   * connected snapshot whose daemon reports stale hooks. The deck's dashboard
   * card shows the notice, so the snapshot mapping carries it (the fixture
   * scenarios above skip that mapping).
   */
  test("is shown from a live daemon snapshot", async ({ page }) => {
    const deckId = "deck-0000000000001637";
    const snapshot = {
      connection: {
        status: "connected", deckKind: "local", deckId, socketPath: "/tmp/hook-notice-live.sock",
        clientProtocolVersion: 10, serverProtocolVersion: 10, clientBuildVersion: "0.46.0", daemonBuildVersion: "0.46.0",
        runningAgentCount: 0,
        hookBinaryNotices: [{
          binary: "/opt/homebrew/bin/dot-agent-deck", agents: ["Claude Code", "Codex"], version: "0.45.1",
          daemonVersion: "0.46.0", reason: "older", remedy: "Run:", command: "brew upgrade dot-agent-deck",
        }],
      },
      agents: [], protocolVersion: 10, source: "daemon", fleet: [deckId],
    };
    // Same IPC callback/event contract as @tauri-apps/api/mocks.mockIPC,
    // installed before the bundle loads so the real live bridge is driven.
    await page.addInitScript((initial) => {
      const callbacks = new Map<number, (event: unknown) => void>();
      let next = 0;
      Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {
        transformCallback: (callback: (event: unknown) => void) => { const id = ++next; callbacks.set(id, callback); return id; },
        unregisterCallback: (id: number) => callbacks.delete(id),
        invoke: async (command: string, args: Record<string, unknown> = {}) => {
          if (command === "plugin:event|listen") return args.handler;
          if (command === "plugin:event|unlisten") return;
          if (command === "desktop_bootstrap") return initial;
          if (command === "desktop_features") return {};
          if (command === "desktop_get_settings") return { settings: { version: 1, appearance: { mode: "system" }, zoom: { level: 1 } } };
          if (command === "desktop_set_zoom") return args.level;
          return { ok: true };
        },
      } });
      Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", { value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) } });
    }, snapshot);
    await page.goto("/?live=1");
    const notice = page.getByTestId("daemon-group").getByTestId("hook-binary-notice");
    await expect(notice).toBeVisible();
    await expect(notice).toContainText("Claude Code, Codex hooks run dot-agent-deck 0.45.1 (/opt/homebrew/bin/dot-agent-deck); this deck is 0.46.0");
    await expect(notice.getByTestId("hook-binary-notice-command")).toHaveText("brew upgrade dot-agent-deck");
  });
});
