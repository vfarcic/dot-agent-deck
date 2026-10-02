import { createServer } from "node:net";

import { defineConfig, devices } from "@playwright/test";

/**
 * The browser-level test tier for the desktop web build (issues #823, #836).
 *
 * **What it drives.** `webServer` below runs `vite build` and then serves the
 * resulting `desktop/dist` with `vite preview`, so every spec under `e2e/`
 * loads the same production bundle `pnpm build` produces — minified, targeted
 * at `safari15`, with the real stylesheet. It is deliberately not the dev
 * server and deliberately not the jsdom tree the vitest suite renders into.
 *
 * **Why two projects.** jsdom computes no geometry, so the ~200 vitest tests
 * can assert what React renders and nothing about what a reader sees. These run
 * in engines that lay out and paint. Chromium is the cheap one; WebKit is here
 * because the app SHIPS on WebKit — WebKitGTK under Tauri on Linux, WKWebView
 * on macOS — and, until this tier existed, every automated check of this app
 * ran in Chromium or in jsdom. Playwright's WebKit is its own build from the
 * WebKit source tree (the binary it launches on Linux is `minibrowser-gtk`,
 * WebKit's GTK port), so it answers WebCore and JavaScriptCore questions the
 * way WebKit answers them. It is NOT the distribution's WebKitGTK, not
 * WKWebView, and not a Tauri window; see `docs/develop/desktop-gui.md` for what
 * that leaves uncovered.
 *
 * **No retries, on purpose.** A retry turns a flake into a green tick and
 * deletes the evidence, which is the failure mode issue #807 documents for the
 * Rust e2e tier on a 4-vCPU runner. The specs wait on state — a locator being
 * visible, the legend printing a chosen column set, a count reaching fifteen —
 * and never on a timer, so a failure here is meant to be a fact rather than a
 * coin toss. `.config/nextest.toml` takes the same position for the Rust tiers.
 */

/**
 * Which port this run serves the bundle on (issue #1481).
 *
 * **A free port per run, asked of the OS.** The port used to be a fixed 4173
 * with `reuseExistingServer` on locally, so a run on a machine where another
 * checkout's `vite preview` already held 4173 drove that checkout's bundle and
 * never built its own — measured: a worktree whose build referenced
 * `index-CuCCDo_1.js` ran its specs against `index-B4khJSat.js`, served by a
 * sibling worktree's preview server left running for twelve hours. Binding
 * port 0 and reading back what the OS chose cannot collide with a port in use
 * at that moment, which a random number can. The port is released before vite
 * binds it, so another process can take it in between; `--strictPort` and
 * `reuseExistingServer: false` below turn that race into a failed run rather
 * than a run against somebody else's server.
 *
 * **Chosen once, in the main process.** Playwright evaluates this file in the
 * runner AND again in every worker, and each evaluation picking its own port
 * would give the workers a `baseURL` nothing serves. So the first evaluation
 * writes its choice into `process.env`, the workers inherit that environment,
 * and every later evaluation reads the same value back.
 *
 * **Reusing a server you started is an explicit opt-in.** With
 * `DAD_BROWSER_REUSE_SERVER=1` (ignored in CI), the run uses whatever already
 * answers on `DAD_BROWSER_PORT` — 4173, vite preview's own default, when that
 * is unset — and starts a server only if nothing does. Setting
 * `DAD_BROWSER_PORT` alone pins the port without reusing anything: a busy port
 * then fails the run.
 */
const PORT_ENV = "DAD_BROWSER_PORT";
const REUSE_SERVER = process.env.DAD_BROWSER_REUSE_SERVER === "1" && !process.env.CI;

function freePort(): Promise<number> {
  return new Promise((settle, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => (typeof address === "object" && address ? settle(address.port) : fail(new Error("no port"))));
    });
  });
}

if (!process.env[PORT_ENV]) {
  process.env[PORT_ENV] = String(REUSE_SERVER ? 4173 : await freePort());
}
const PORT = Number(process.env[PORT_ENV]);
if (!Number.isInteger(PORT) || PORT < 1 || PORT > 65_535) {
  throw new Error(`${PORT_ENV} is not a port: ${process.env[PORT_ENV]}`);
}
const PREVIEW = `vite preview --port ${PORT} --strictPort --host 127.0.0.1`;

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  // A `test.only` left in a spec silently shrinks the suite to one test, and
  // the job still reports green. Fail the run instead, but only in CI, where
  // narrowing to one test is never what the author meant.
  forbidOnly: !!process.env.CI,
  retries: 0,
  // One worker in CI to start. Layout is not timing-dependent, but browser
  // startup on a 4-vCPU runner contends with itself, and this job has no honest
  // runs yet — the same reason `e2e-deterministic` was added advisory. Raising
  // it is a deliberate act once the job has a run history to argue from.
  workers: process.env.CI ? 1 : undefined,
  reporter: process.env.CI
    ? [["github"], ["html", { open: "never" }], ["list"]]
    : [["list"]],
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    // Failure-only, so a green run writes nothing and a red one hands CI a
    // trace to open. The CI job uploads both directories.
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    // The first WebKit launch on a runner is slow enough to eat a spec's whole
    // budget, so it happens here first, outside any spec. The file's own header
    // has the measurements.
    { name: "webkit-warm-up", testMatch: /webkit-warm-up\.setup\.ts/, use: { ...devices["Desktop Safari"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] }, dependencies: ["webkit-warm-up"] },
  ],
  webServer: {
    // Build then serve, as one command, so there is no ordering trap where a
    // stale `dist` is tested and the run still passes. `--strictPort` makes a
    // busy port an error rather than a silent move to another one, which would
    // leave `baseURL` pointing at whatever else is listening — the race the
    // port's comment above describes.
    //
    // The local `vite` bin, not `pnpm exec vite`, and `exec` for the server:
    // Playwright stops the web server by killing the process group of the
    // shell it spawned, and then waits for that server to exit. pnpm 12.6.0's
    // `exec` starts its child in a process group of its own, so the kill
    // missed `vite preview`, which then kept running, and the suite hung after
    // its last test until CI's 20-minute timeout cancelled it. Measured:
    // under pnpm 12.5.1 a three-test spec exited in 5s; under 12.6.0 the same
    // spec passed and then never exited. `exec` makes the shell itself become
    // the server, so the group Playwright kills is the server's own.
    //
    // `exec` is a POSIX shell builtin and `cmd.exe` rejects it, so on Windows
    // the build would finish and the preview server would never start —
    // Playwright would then wait for the port until it timed out (Qodo, PR
    // #1269). CI runs this tier on `ubuntu-latest` only, but the browser tier
    // needs no daemon, so running it locally on Windows is a reasonable thing
    // to do and must not be broken by a hang fix for a different platform.
    // Windows keeps the pre-#1269 form: pnpm's process-group behaviour is what
    // this works around, and the workaround is only correct where `exec` is.
    command:
      process.platform === "win32"
        ? `pnpm exec vite build && pnpm exec ${PREVIEW}`
        : `./node_modules/.bin/vite build && exec ./node_modules/.bin/${PREVIEW}`,
    url: `http://127.0.0.1:${PORT}/`,
    // `false` unless `DAD_BROWSER_REUSE_SERVER=1` (see the port's comment
    // above), so a run builds and serves its own bundle and a port something
    // else already answers on fails it with Playwright's "is already used"
    // error. `--strictPort` alone could not give that: it governs only a
    // server THIS config starts, and with reuse on, a server already listening
    // means this config starts none. With the opt-in, whatever answers on the
    // port is what the suite drives — the build in this command does not run,
    // so the bundle under test is as current as the server you started.
    reuseExistingServer: REUSE_SERVER,
    // The build is inside this command, so the default 60s is not enough on a
    // cold runner.
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
