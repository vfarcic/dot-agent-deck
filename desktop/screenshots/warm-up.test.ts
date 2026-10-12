import { describe, expect, it } from "vitest";

import { warmUpWith, type WarmUpBrowser, type WarmUpLog, type WarmUpPage } from "./warm-up";

/**
 * PR #1672 review: the docs-screenshot warm-up must warn and let the run go on
 * whatever fails, because a throw from Playwright's `globalSetup` aborts every
 * shot. Each case stands a fake browser in for Chromium; collected by `pnpm
 * test` (vitest), since the screenshot config runs only `*.shot.ts`.
 */

interface Recorded {
  log: WarmUpLog;
  logs: string[];
  warnings: string[];
}

function recorder(): Recorded {
  const logs: string[] = [];
  const warnings: string[] = [];
  return {
    logs,
    warnings,
    log: { log: (m) => logs.push(m), warn: (m) => warnings.push(m) },
  };
}

interface FakeOptions {
  goto?: () => Promise<unknown>;
  evaluate?: () => Promise<void>;
  close?: () => Promise<void>;
}

function fakeBrowser(options: FakeOptions = {}): WarmUpBrowser & { closed: number; visited: string[] } {
  const visited: string[] = [];
  const page: WarmUpPage = {
    setDefaultTimeout: () => undefined,
    goto: async (url) => {
      visited.push(url);
      return options.goto ? options.goto() : null;
    },
    setContent: async () => {
      visited.push("<stand-in>");
    },
    evaluate: () => (options.evaluate ? options.evaluate() : Promise.resolve()),
  };
  const browser = {
    closed: 0,
    visited,
    newPage: async () => page,
    close: async () => {
      browser.closed += 1;
      if (options.close) await options.close();
    },
  };
  return browser;
}

describe("the docs-screenshot warm-up", () => {
  it("logs how long settled fonts took and closes its browser", async () => {
    const browser = fakeBrowser();
    const r = recorder();
    const outcome = await warmUpWith({ launch: async () => browser, url: "file:///a-tui.html", budgetMs: 1_000, log: r.log });
    expect(outcome.kind).toBe("settled");
    expect(browser.visited).toEqual(["file:///a-tui.html"]);
    expect(browser.closed).toBe(1);
    expect(r.warnings).toEqual([]);
    expect(r.logs).toEqual([expect.stringMatching(/to settled fonts$/)]);
  });

  it("warns and continues when the fonts wait itself rejects", async () => {
    const browser = fakeBrowser({ evaluate: () => Promise.reject(new Error("Target page, context or browser has been closed")) });
    const r = recorder();
    const outcome = await warmUpWith({ launch: async () => browser, url: undefined, budgetMs: 1_000, log: r.log });
    expect(outcome.kind).toBe("failed");
    expect(browser.closed).toBe(1);
    expect(r.warnings).toEqual([expect.stringContaining("has been closed")]);
  });

  it("warns and continues when the page does not load", async () => {
    const browser = fakeBrowser({ goto: () => Promise.reject(new Error("net::ERR_FILE_NOT_FOUND")) });
    const r = recorder();
    const outcome = await warmUpWith({ launch: async () => browser, url: "file:///gone-tui.html", budgetMs: 1_000, log: r.log });
    expect(outcome.kind).toBe("failed");
    expect(browser.closed).toBe(1);
    expect(r.warnings).toEqual([expect.stringContaining("ERR_FILE_NOT_FOUND")]);
  });

  it("warns and continues when the browser does not launch", async () => {
    const r = recorder();
    const outcome = await warmUpWith({
      launch: () => Promise.reject(new Error("Executable doesn't exist")),
      url: undefined,
      budgetMs: 1_000,
      log: r.log,
    });
    expect(outcome.kind).toBe("failed");
    expect(r.warnings).toEqual([expect.stringContaining("Executable doesn't exist")]);
  });

  it("warns and continues when the fonts outlast the budget, and closes the browser", async () => {
    const browser = fakeBrowser({ evaluate: () => new Promise<void>(() => undefined) });
    const r = recorder();
    const outcome = await warmUpWith({ launch: async () => browser, url: undefined, budgetMs: 20, log: r.log });
    expect(outcome.kind).toBe("timed-out");
    expect(browser.closed).toBe(1);
    expect(r.warnings).toEqual([expect.stringContaining("had not settled after 20ms")]);
  });

  it("keeps the fonts' answer when closing the browser fails", async () => {
    const browser = fakeBrowser({ close: () => Promise.reject(new Error("close failed")) });
    const r = recorder();
    const outcome = await warmUpWith({ launch: async () => browser, url: undefined, budgetMs: 1_000, log: r.log });
    expect(outcome.kind).toBe("settled");
    expect(r.warnings).toEqual([expect.stringContaining("close failed")]);
  });
});
