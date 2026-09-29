import { expect, test, type Page } from "@playwright/test";
import { enterDeck } from "./support/overview";

interface RecordingWindow {
  __dadE2eTerminals?: { element?: HTMLElement; onData(listener: (data: string) => void): unknown }[];
  __dadE2eSent?: string[];
}

/**
 * Capture every xterm instance as the app registers it, the same way
 * `agent-pane-overlay.spec.ts` does, so a spec can listen to what a terminal
 * hands the app for the agent's PTY.
 */
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

/**
 * Make the page report `platform` before the app loads, the way each shipped
 * webview reports its own: WKWebView says `MacIntel`, WebView2 `Win32`,
 * WebKitGTK `Linux x86_64`. xterm.js reads the same property, so its own
 * macOS handling follows too. Where the engine also has `userAgentData`
 * (Chromium, as WebView2 does), its platform is made to agree: otherwise it
 * would still say what the Desktop Chrome device's Windows user agent implies.
 */
async function reportPlatform(page: Page, platform: "MacIntel" | "Win32" | "Linux x86_64"): Promise<void> {
  await page.addInitScript((reported) => {
    Object.defineProperty(Navigator.prototype, "platform", { configurable: true, get: () => reported });
    if ("userAgentData" in Navigator.prototype) {
      const hint = reported === "MacIntel" ? "macOS" : reported === "Win32" ? "Windows" : "Linux";
      Object.defineProperty(Navigator.prototype, "userAgentData", { configurable: true, get: () => ({ platform: hint }) });
    }
  }, platform);
}

/**
 * Open the crowded deck, focus the writable agent's terminal, and return a
 * function that presses one chord with the engine's own key events and
 * resolves to exactly what the terminal handed the app for the agent's PTY.
 */
async function openWritableTerminal(page: Page): Promise<(chord: string) => Promise<string[]>> {
  await captureTerminals(page);
  await page.goto("/?fixture=1&state=crowded");
  await enterDeck(page);

  const viewport = page.getByTestId("terminal-2");
  await expect(viewport).toHaveAttribute("aria-disabled", "false");
  await viewport.locator("textarea.xterm-helper-textarea").focus();
  await viewport.evaluate((root) => {
    const recording = window as Window & RecordingWindow;
    const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
    if (!terminal) throw new Error("the writable tile's xterm was not captured");
    recording.__dadE2eSent = [];
    terminal.onData((data) => recording.__dadE2eSent?.push(data));
  });

  return async (chord: string) => {
    await page.evaluate(() => { (window as Window & RecordingWindow).__dadE2eSent = []; });
    await page.keyboard.press(chord);
    return page.evaluate(() => (window as Window & RecordingWindow).__dadE2eSent ?? []);
  };
}

/**
 * The deck's terminal-input contract, exercised against the crowded fixture so
 * the rendered page includes coordinator, writable, history-only, and
 * no-live-target tiles without a daemon or an agent credential.
 */
test.describe("agent terminal input", () => {
  /**
   * Scenario: open the crowded deck and wait for all fifteen xterm inputs to
   * mount. Each tile has exactly that one textarea, including the orchestrator;
   * no role-labelled composer or Send control is rendered beside it.
   */
  test("renders xterm as each tile's only text input", async ({ page }) => {
    await page.goto("/?fixture=1&state=crowded");
    await enterDeck(page);

    const tiles = page.locator(".agent-tile");
    const terminalInputs = tiles.locator("textarea.xterm-helper-textarea");
    await expect(tiles).toHaveCount(15);
    await expect(terminalInputs).toHaveCount(15);
    await expect(tiles.locator('[data-testid^="composer-"]')).toHaveCount(0);
    await expect(tiles.locator("textarea:not(.xterm-helper-textarea)")).toHaveCount(0);
    await expect(tiles.getByText("Message orchestrator", { exact: true })).toHaveCount(0);
    await expect(tiles.getByRole("button", { name: "Send", exact: true })).toHaveCount(0);
  });

  /**
   * Scenario: open the crowded deck, which carries one live writer, one
   * history-only pane, and one pane with no live target. The writer stays
   * enabled while both non-live terminal surfaces are disabled, marked with
   * their exact verdict, and accompanied by a human-readable status.
   */
  test("marks non-live fixture panes instead of accepting terminal input", async ({ page }) => {
    await page.goto("/?fixture=1&state=crowded");
    await enterDeck(page);

    const historyOnly = page.getByTestId("terminal-11");
    await expect(historyOnly).toHaveAttribute("data-input-state", "history-only");
    await expect(historyOnly).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByTestId("terminal-input-status-11")).toHaveAttribute("role", "status");
    await expect(page.getByTestId("terminal-input-status-11")).toHaveText(
      "Terminal input unavailable — the agent has no live pane — only its history remains.",
    );

    const noLiveTarget = page.getByTestId("terminal-15");
    await expect(noLiveTarget).toHaveAttribute("data-input-state", "no-live-target");
    await expect(noLiveTarget).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByTestId("terminal-input-status-15")).toHaveAttribute("role", "status");
    await expect(page.getByTestId("terminal-input-status-15")).toHaveText(
      "Terminal input unavailable — there is nothing live to write to.",
    );

    const applied = page.getByTestId("terminal-2");
    await expect(applied).toHaveAttribute("data-input-state", "applied");
    await expect(applied).toHaveAttribute("aria-disabled", "false");
    await expect(page.getByTestId("terminal-input-status-2")).toHaveCount(0);
  });

  /**
   * Scenario: open the crowded deck, click into the writable agent's terminal
   * and press Enter, Shift+Enter, Ctrl+Enter and Ctrl+/ with the browser's own
   * key events. Each reaches the agent as the TUI sends it: Enter as the
   * carriage return that submits, Shift+Enter and Ctrl+Enter as sequences the
   * agent can tell apart from it rather than that same carriage return, and
   * Escape to the agent (issue #1422).
   */
  test("forwards modified Enter as the TUI does, not as Enter", async ({ page }) => {
    const sentFor = await openWritableTerminal(page);

    expect(await sentFor("Enter")).toEqual(["\r"]);
    expect(await sentFor("Shift+Enter")).toEqual(["\x1b[13;2u"]);
    expect(await sentFor("Control+Enter")).toEqual(["\x1b[13;5u"]);
    expect(await sentFor("Control+Shift+Enter")).toEqual(["\x1b[13;6u"]);
    expect(await sentFor("Alt+Enter")).toEqual(["\x1b\r"]);
    expect(await sentFor("Control+/")).toEqual(["\x1f"]);
    expect(await sentFor("Escape")).toEqual(["\x1b"]);
  });

  /**
   * Scenario: with the webview reporting macOS, click into the writable
   * agent's terminal and press macOS's line-editing shortcuts with the
   * browser's own key events: Cmd+Left/Right, Option+Left/Right,
   * Cmd+Backspace, Option+Backspace and Option+Delete. Each reaches the agent
   * as bytes every supported agent's input box acts on (issue #1422).
   */
  test("sends macOS's editing shortcuts as the agents' line editors expect", async ({ page }) => {
    await reportPlatform(page, "MacIntel");
    const sentFor = await openWritableTerminal(page);

    expect(await sentFor("Meta+ArrowLeft")).toEqual(["\x01"]);
    expect(await sentFor("Meta+ArrowRight")).toEqual(["\x05"]);
    expect(await sentFor("Alt+ArrowLeft")).toEqual(["\x1b[1;3D"]);
    expect(await sentFor("Alt+ArrowRight")).toEqual(["\x1b[1;3C"]);
    expect(await sentFor("Meta+Backspace")).toEqual(["\x15"]);
    expect(await sentFor("Alt+Backspace")).toEqual(["\x1b\x7f"]);
    expect(await sentFor("Alt+Delete")).toEqual(["\x1bd"]);
    expect(await sentFor("Backspace")).toEqual(["\x7f"]);
  });

  /**
   * Scenario: with the webview reporting Windows, and then Linux, press that
   * platform's line-editing shortcuts in the writable agent's terminal: Home,
   * End, Ctrl+Left/Right, Ctrl+Backspace and Ctrl+Delete. Each reaches the
   * agent as bytes every supported agent's input box acts on (issue #1422).
   */
  for (const platform of ["Win32", "Linux x86_64"] as const) {
    test(`sends ${platform}'s editing shortcuts as the agents' line editors expect`, async ({ page }) => {
      await reportPlatform(page, platform);
      const sentFor = await openWritableTerminal(page);

      expect(await sentFor("Home")).toEqual(["\x1b[H"]);
      expect(await sentFor("End")).toEqual(["\x1b[F"]);
      expect(await sentFor("Control+ArrowLeft")).toEqual(["\x1b[1;5D"]);
      expect(await sentFor("Control+ArrowRight")).toEqual(["\x1b[1;5C"]);
      expect(await sentFor("Control+Backspace")).toEqual(["\x17"]);
      expect(await sentFor("Control+Delete")).toEqual(["\x1bd"]);
      expect(await sentFor("Backspace")).toEqual(["\x7f"]);
    });
  }

  /**
   * Scenario: put text on the clipboard, then press the paste shortcut of
   * Windows (Ctrl+V, Ctrl+Shift+V) or Linux (Ctrl+Shift+V) in the writable
   * agent's terminal. The clipboard's text reaches the agent, and nothing else
   * does: in particular Windows' Ctrl+V is not sent as ^V (issue #1422).
   */
  for (const [platform, chord] of [
    ["Win32", "Control+v"],
    ["Win32", "Control+Shift+V"],
    ["Linux x86_64", "Control+Shift+V"],
  ] as const) {
    test(`pastes the clipboard with ${chord} on ${platform}`, async ({ page, context, browserName }) => {
      await reportPlatform(page, platform);
      const sentFor = await openWritableTerminal(page);
      if (browserName === "chromium") await context.grantPermissions(["clipboard-read", "clipboard-write"]);
      await page.evaluate(() => navigator.clipboard.writeText("dad-paste-1422"));
      expect(await sentFor(chord)).toEqual(["dad-paste-1422"]);
    });
  }

  /**
   * Scenario: with the webview reporting macOS, press Cmd+V in the writable
   * agent's terminal. Nothing is sent to the agent for the key itself and the
   * key is not cancelled, so the webview's own paste can go ahead. (These
   * engines run on a Linux host, whose editing keys do not paste on Cmd+V, so
   * the paste itself is not observable here.)
   */
  test("leaves macOS's Cmd+V to the webview", async ({ page }) => {
    await reportPlatform(page, "MacIntel");
    const sentFor = await openWritableTerminal(page);
    await page.evaluate(() => {
      const recording = window as Window & { __dadE2eCancelled?: boolean[] };
      recording.__dadE2eCancelled = [];
      // Bubble phase on the window, after xterm and its key handler ran.
      window.addEventListener("keydown", (event) => recording.__dadE2eCancelled?.push(event.defaultPrevented));
    });
    expect(await sentFor("Meta+v")).toEqual([]);
    expect(await page.evaluate(() => (window as Window & { __dadE2eCancelled?: boolean[] }).__dadE2eCancelled)).toEqual([false, false]);
  });

  /**
   * Scenario: press Ctrl+V in the writable agent's terminal with the webview
   * reporting macOS, and then Linux. There Ctrl+V is not the paste shortcut
   * but the agent's own (Claude Code, for one, pastes an image with it), so it
   * reaches the agent as ^V (issue #1422).
   */
  for (const platform of ["MacIntel", "Linux x86_64"] as const) {
    test(`keeps Ctrl+V for the agent on ${platform}`, async ({ page }) => {
      await reportPlatform(page, platform);
      const sentFor = await openWritableTerminal(page);
      expect(await sentFor("Control+v")).toEqual(["\x16"]);
    });
  }
});
