import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(cleanup);

// How long `findBy*` and `waitFor` keep polling before they fail. A ceiling,
// not a pace: each returns as soon as its condition holds. The default of 1s
// is an idle machine's budget; under heavy parallel load (measured 2026-10-01,
// load averages of 90 to 130 on 16 cores) a pane opened through the voice
// flow, which waits on the 250ms voice-status poll, was not on screen within
// it. See `testTimeout` in `vite.config.ts`, which this has to stay below.
configure({ asyncUtilTimeout: 5_000 });

class TestResizeObserver implements ResizeObserver {
  readonly root = null;
  readonly rootMargin = "";
  readonly thresholds = [];
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords(): ResizeObserverEntry[] { return []; }
}

Object.defineProperty(window, "ResizeObserver", { value: TestResizeObserver, writable: true });
Object.defineProperty(globalThis, "ResizeObserver", { value: TestResizeObserver, writable: true });
Object.defineProperty(window, "requestAnimationFrame", { value: (callback: FrameRequestCallback) => window.setTimeout(() => callback(performance.now()), 0), writable: true });
Object.defineProperty(window, "cancelAnimationFrame", { value: (id: number) => window.clearTimeout(id), writable: true });
