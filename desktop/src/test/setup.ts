import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(cleanup);

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
