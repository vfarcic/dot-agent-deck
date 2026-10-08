import { availableParallelism, loadavg } from "node:os";
import { test as base } from "@playwright/test";

/**
 * A bounded outer budget for browser startup, steps and context teardown.
 * The affected specs exceeded their 30s budget at loads of 68 and 93 on a
 * 16-core host, including a WebKit failure during teardown after assertions.
 * Locator assertions retain Playwright's normal 5s bound and retries stay off.
 */
export const test = base.extend<{ loadBudget: void }>({
  loadBudget: [async ({}, use, info) => {
    const ratio = loadavg()[0] / availableParallelism();
    const factor = Math.min(6, Math.max(1, Number.isFinite(ratio) ? ratio : 1));
    info.setTimeout(Math.ceil(info.timeout * factor));
    await use();
  }, { auto: true }],
});
