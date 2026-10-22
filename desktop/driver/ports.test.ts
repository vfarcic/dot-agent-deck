// Issue #1795 — the driver harness's port allocation, checked without a
// window, a daemon or a native driver. tauri-driver given its own port as
// `--native-port` forwards every request to itself, so the two ports
// `startWindow` asks for must differ; this is the check that they do.

import assert from "node:assert/strict";
import { createServer } from "node:net";
import { test } from "node:test";
import { freePorts } from "./harness.ts";

/** Bind `port` on loopback and release it again, or throw if it is taken. */
function bindAndRelease(port: number): Promise<void> {
  return new Promise((settle, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(port, "127.0.0.1", () => server.close(() => settle()));
  });
}

/// Scenario: ask the harness for two free ports 20,000 times and see that the
/// two are never the same port, then bind each port of one more pair to see
/// that the harness released both. Choosing each port with its own bind and
/// close repeated a port 3 times in 20,000 on Linux, so this loop catches that
/// regression in about 95% of runs, not every run.
test("ports_001 the two ports the window's drivers get are always different, and released", async () => {
  for (let i = 0; i < 20_000; i++) {
    const [port, nativePort] = await freePorts(2);
    assert.notEqual(port, nativePort, `freePorts returned ${port} twice on attempt ${i + 1}`);
  }
  for (const port of await freePorts(2)) await bindAndRelease(port);
});
