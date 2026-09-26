// Issue #953 — the driver tier's harness: one isolated sandbox, one real
// daemon, one tauri-driver and one real app window per scenario.
//
// EVERY WAIT HERE IS ON STATE. `waitFor` polls a probe until the probe reports
// the condition, and its deadline is a bound on a hang, never the thing a pass
// depends on — issue #807's load-sensitive race in the Rust e2e tier is what a
// "sleep 2s, then look" produces on a busy 4-vCPU runner, and a native window
// carries strictly more of that exposure than the headless browser tier does.
// There is deliberately no `sleep` export: a scenario that needs one is waiting
// on the wrong thing.

import { type ChildProcess, execFile, spawn } from "node:child_process";
import { closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { type Element, Session, serverReady, WAIT_MS } from "./webdriver.ts";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/**
 * The bound on any one wait. Generous on purpose: a passing run never reaches
 * it, so raising it costs nothing but the time a genuine hang takes to report.
 * 120s because the first GitHub runner measurement ran each scenario in
 * ~37-38s end to end against 6-10s on a 16-core dev box: a bound sized from
 * the dev box would leave a slow runner a fraction of that margin. Parsed and
 * validated in `webdriver.ts`, which bounds every request by the same value.
 */
export { WAIT_MS };

/** Polling cadence inside `waitFor`. Not a wait in its own right. */
const POLL_MS = 100;

const paths = {
  app: process.env.DAD_DRIVER_APP ?? join(REPO_ROOT, "target", "debug", "dot-agent-deck-desktop"),
  daemon: process.env.DAD_DRIVER_DAEMON ?? join(REPO_ROOT, "target", "debug", "dot-agent-deck"),
  tauriDriver: process.env.DAD_DRIVER_TAURI_DRIVER ?? "tauri-driver",
  nativeDriver: process.env.DAD_DRIVER_NATIVE_DRIVER ?? "WebKitWebDriver",
  results: process.env.DAD_DRIVER_RESULTS ?? join(REPO_ROOT, "desktop", "driver-results"),
};

/**
 * Poll `probe` until it returns something other than `undefined`, `null` or
 * `false`, and return that. On the deadline, throw with the last thing the
 * probe saw, so a failure says what state the window was actually in.
 */
export async function waitFor<T>(
  description: string,
  probe: () => Promise<T | undefined | null | false>,
  timeoutMs = WAIT_MS,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown;
  for (;;) {
    try {
      const value = await probe();
      if (value !== undefined && value !== null && value !== false) return value;
      lastError = undefined;
    } catch (error) {
      lastError = error;
    }
    if (Date.now() >= deadline) {
      const detail = lastError instanceof Error ? ` (last error: ${lastError.message})` : "";
      throw new Error(`timed out after ${timeoutMs}ms waiting for ${description}${detail}`);
    }
    await new Promise((settle) => setTimeout(settle, POLL_MS));
  }
}

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

function run(file: string, args: string[], env: NodeJS.ProcessEnv): Promise<{ code: number; stdout: string }> {
  return new Promise((settle) => {
    execFile(file, args, { env, timeout: WAIT_MS }, (error, stdout) => {
      const code = error ? (typeof error.code === "number" ? error.code : 1) : 0;
      settle({ code, stdout });
    });
  });
}

/**
 * Spawn `file` and fail fast if it cannot start: a missing binary is an
 * `error` event rather than an exit, and without this a readiness probe
 * would poll a process that never existed until its deadline.
 */
function launch(file: string, args: string[], env: NodeJS.ProcessEnv, log: string): ChildProcess {
  const fd = openSync(log, "a");
  const child = spawn(file, args, { env, stdio: ["ignore", fd, fd] });
  closeSync(fd);
  child.once("error", (error) => {
    (child as ChildProcess & { launchError?: Error }).launchError = error;
  });
  return child;
}

/** Throw if `child` failed to start or has exited — for use inside a readiness probe. */
function assertRunning(child: ChildProcess | undefined, name: string): void {
  const launchError = (child as (ChildProcess & { launchError?: Error }) | undefined)?.launchError;
  if (launchError) throw new Error(`${name} failed to start: ${launchError.message}`);
  if (child?.exitCode !== null || child?.signalCode !== null) {
    throw new Error(`${name} exited (${child?.exitCode ?? child?.signalCode})`);
  }
}

function exited(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((settle) => child.once("exit", () => settle()));
}

/** Stop a child this harness spawned, by its own pid and nothing broader. */
async function stopChild(child: ChildProcess | undefined): Promise<void> {
  if (!child || child.pid === undefined) return;
  if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
  const gone = await Promise.race([exited(child).then(() => true), new Promise<boolean>((s) => setTimeout(() => s(false), 10_000))]);
  if (!gone) {
    child.kill("SIGKILL");
    await exited(child);
  }
}

/** What a scenario's deck is configured with. */
export type DeckOptions = {
  /** Start the daemon before the window opens. `false` leaves it to `startDaemon()`. */
  daemonFirst: boolean;
  /** `default_command` in the deck's `config.toml`, which seeds New agent's Command field. */
  defaultCommand?: string;
};

export class Deck {
  readonly sandbox: string;
  /** A directory inside the sandbox, canonical, that the deck offers as its `default_dir`. */
  readonly project: string;
  readonly env: NodeJS.ProcessEnv;
  session!: Session;
  private daemon?: ChildProcess;
  private driver?: ChildProcess;

  private constructor(sandbox: string, options: DeckOptions) {
    this.sandbox = sandbox;
    this.project = join(sandbox, "driver-project");
    mkdirSync(this.project);
    mkdirSync(join(sandbox, "home"));
    mkdirSync(join(sandbox, "state"));

    const config = [`default_dir = ${JSON.stringify(this.project)}`];
    if (options.defaultCommand !== undefined) config.push(`default_command = ${JSON.stringify(options.defaultCommand)}`);
    writeFileSync(join(sandbox, "config.toml"), `${config.join("\n")}\n`);

    // Nothing ambient named DOT_AGENT_DECK_* may reach the sandbox: run from an
    // agent pane, the parent carries that pane's own ids and sockets, and one
    // stray socket variable is enough to put this window on a real deck.
    const inherited = Object.fromEntries(
      Object.entries(process.env).filter(([name]) => !name.startsWith("DOT_AGENT_DECK_")),
    );
    const home = join(sandbox, "home");
    this.env = {
      ...inherited,
      HOME: home,
      XDG_CONFIG_HOME: join(home, ".config"),
      XDG_CACHE_HOME: join(home, ".cache"),
      XDG_DATA_HOME: join(home, ".local", "share"),
      XDG_STATE_HOME: join(home, ".local", "state"),
      // The six that `docs/develop/local-run.md` says a sandbox deck needs, so
      // neither side of this test can find the operator's real deck.
      DOT_AGENT_DECK_ATTACH_SOCKET: join(sandbox, "attach.sock"),
      DOT_AGENT_DECK_SOCKET: join(sandbox, "hook.sock"),
      DOT_AGENT_DECK_STATE_DIR: join(sandbox, "state"),
      DOT_AGENT_DECK_SESSION: join(sandbox, "session.toml"),
      DOT_AGENT_DECK_SCHEDULES: join(sandbox, "schedules.toml"),
      DOT_AGENT_DECK_LOG: join(sandbox, "deck.log"),
      DOT_AGENT_DECK_DESKTOP_CONFIG: join(sandbox, "desktop.toml"),
      DOT_AGENT_DECK_CONFIG: join(sandbox, "config.toml"),
      DOT_AGENT_DECK_BINARY: paths.daemon,
      // Pinned rather than read from a config file: the shipped default is
      // what these scenarios are about.
      DOT_AGENT_DECK_EXPERIMENTAL: "0",
      // The daemon starts before anything attaches to it, which is exactly
      // the window CLAUDE.md rule 12 names: its 30s idle shutdown would let a
      // slow runner time out a daemon the window was about to connect to.
      DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS: "0",
      // A backstop, not a timing assumption: a daemon this harness failed to
      // stop still exits on its own.
      DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS: "900",
    };
  }

  /** Build the sandbox, start what `options` asks for, and open the real window on it. */
  static async open(options: DeckOptions, name: string): Promise<Deck> {
    if (!process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
      throw new Error("no display: run the driver tier under `xvfb-run -a` (see docs/develop/desktop-gui.md)");
    }
    const deck = new Deck(realpathSync(mkdtempSync(join(tmpdir(), "dad-driver-"))), options);
    try {
      if (options.daemonFirst) await deck.startDaemon();
      await deck.startWindow();
    } catch (error) {
      await deck.close(error, name);
      throw error;
    }
    return deck;
  }

  /** Start `daemon serve` under the sandbox and wait until it answers a Hello. */
  async startDaemon(): Promise<void> {
    this.daemon = launch(paths.daemon, ["daemon", "serve"], this.env, join(this.sandbox, "daemon.out"));
    // `daemon endpoint` prints only after a bounded attach-protocol Hello has
    // succeeded against the socket, so a zero exit is the daemon being ready,
    // not merely its socket file existing.
    await waitFor("the sandbox daemon to answer a Hello", async () => {
      assertRunning(this.daemon, "the daemon");
      return (await run(paths.daemon, ["daemon", "endpoint"], this.env)).code === 0;
    });
  }

  private async startWindow(): Promise<void> {
    const port = await freePort();
    const nativePort = await freePort();
    // The app is launched by the native driver, which tauri-driver launches,
    // so this environment is the one the window runs under.
    this.driver = launch(
      paths.tauriDriver,
      ["--port", String(port), "--native-port", String(nativePort), "--native-driver", paths.nativeDriver],
      this.env,
      join(this.sandbox, "driver.log"),
    );
    const base = `http://127.0.0.1:${port}`;
    await waitFor("tauri-driver to accept sessions", async () => {
      assertRunning(this.driver, "tauri-driver");
      return serverReady(base);
    });
    this.session = await Session.open(base, paths.app);
    // Past `about:blank` first: the webview's initial empty document is
    // complete, and already carries the Tauri internals, before the app's own
    // URL has loaded — so a wait on those alone can pass on a page about to
    // be replaced.
    // A window that never leaves `about:blank` is almost always the binary
    // below: its failed dev-server load leaves WebKit's own error page
    // ("Could not connect to localhost") over a still-blank location, which
    // is what this was measured showing — hence the hint in the description.
    const href = await waitFor(
      "the window to navigate to the app (stuck on about:blank? the binary is probably a plain-cargo build " +
        "that loads the dev server — `cargo test-fast` rebuilds it; rerun driver-test.sh without --no-build)",
      () =>
      this.session.execute<string | null>(
        "return location.href !== 'about:blank' && document.readyState === 'complete' ? location.href : null",
      ),
    );
    // A plain `cargo build` of the desktop crate — which `cargo test-fast`
    // does, since the crate is a workspace member — writes a binary WITHOUT
    // Tauri's custom protocol over the same path, and that binary loads the
    // dev server's URL instead of the embedded bundle. Measured: every wait
    // then timed out on a "Could not connect to localhost" page. The
    // navigation wait above names the usual shape; this names the other.
    if (!href.startsWith("tauri://")) {
      throw new Error(
        `the window loaded ${href}, not the embedded bundle: ${paths.app} was built by plain cargo ` +
          "(cargo test-fast rebuilds it). Rebuild with `sh ./scripts/driver-test.sh` (no --no-build)",
      );
    }
    // The live bridge, not the fixture: a plain browser would fall back to
    // fixture transport, and every assertion after this would be about data
    // the fixture invented.
    await waitFor("the Tauri bridge in the loaded page", () =>
      this.session.execute<boolean>("return !!window.__TAURI_INTERNALS__"),
    );
    // WebKitWebDriver places a pointer in CSS pixels without applying the
    // scale WebKitGTK derives from the screen's DPI, so on a scaled display
    // every click lands short of its target — measured under xvfb's default
    // 100 DPI: devicePixelRatio 1.041 (= 100/96), and a click on the New agent
    // button arriving 46px to its left, on the header. Refuse that here, by
    // name, rather than let it surface as "the dialog never opened".
    const ratio = await this.session.execute<number>("return window.devicePixelRatio");
    if (ratio !== 1) {
      throw new Error(
        `devicePixelRatio is ${ratio}, and WebKitWebDriver misplaces clicks on a scaled display: ` +
          'run xvfb at 96 DPI (xvfb-run -a -s "-screen 0 1920x1080x24 -dpi 96")',
      );
    }
  }

  /** The element matching `css` once it exists. */
  element(css: string, description = css): Promise<Element> {
    return waitFor(description, () => this.session.find(css));
  }

  /** The element's visible text once `accept` holds for it. */
  async textOf(css: string, accept: (text: string) => boolean, description: string): Promise<string> {
    let last: string | undefined;
    try {
      return await waitFor(description, async () => {
        // `innerText`, read in the page, rather than WebDriver's Get Element
        // Text: WebKitWebDriver's visibility atom answered "" for text plainly
        // painted inside the New agent dialog. `innerText` is still the
        // rendered text — it is layout-aware and skips what CSS hides.
        const text = await this.session.execute<string | null>(
          "const e = document.querySelector(arguments[0]); return e ? e.innerText : null",
          [css],
        );
        if (text === null) return undefined;
        last = text;
        return accept(text) ? text : undefined;
      });
    } catch (error) {
      throw new Error(`${(error as Error).message}; last text seen: ${JSON.stringify(last)}`);
    }
  }

  /**
   * The resolved screen text of every mounted terminal, read from xterm's own
   * buffer through the build-time seam in `src/lib/terminalRegistry.ts`. The
   * WebGL renderer leaves no row text in the DOM, so without the seam there is
   * nothing to read.
   */
  async terminalTexts(): Promise<{ key: string; text: string }[]> {
    const texts = await this.session.execute<{ key: string; text: string }[] | null>(
      "return window.__dadDriver ? window.__dadDriver.terminalTexts() : null",
    );
    if (texts === null) {
      throw new Error("the bundle carries no driver seam: build it with VITE_DAD_DRIVER_SEAM=1 (docs/develop/desktop-gui.md)");
    }
    return texts;
  }

  /** The daemon's own view of its agents, from the CLI rather than the window. */
  async daemonAgents(): Promise<{ cwd?: string; label?: string }[]> {
    const { code, stdout } = await run(paths.daemon, ["daemon", "status", "--json"], this.env);
    if (code !== 0) throw new Error(`daemon status exited ${code}`);
    return (JSON.parse(stdout) as { agents: { cwd?: string; label?: string }[] }).agents;
  }

  /**
   * Tear everything down. On failure, first keep what explains it: a
   * screenshot, the page's text and every log, under `driver-results/`.
   */
  async close(failure?: unknown, name = "unnamed"): Promise<void> {
    if (failure !== undefined) await this.preserve(name, failure);
    try {
      await this.session?.close();
    } catch {
      // The window may already be gone; the driver stop below still runs.
    }
    await stopChild(this.driver);
    if (this.daemon) {
      // `daemon stop` identifies the daemon by the peer pid of the sandbox
      // socket it connects to, so it cannot reach any other deck on the box.
      await run(paths.daemon, ["daemon", "stop", "--force"], this.env);
      await stopChild(this.daemon);
    }
    if (failure === undefined) rmSync(this.sandbox, { recursive: true, force: true });
  }

  private async preserve(name: string, failure: unknown): Promise<void> {
    const dir = join(paths.results, name.replace(/[^a-z0-9-]+/gi, "-"));
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "failure.txt"), `${failure instanceof Error ? failure.stack : String(failure)}\n`);
    try {
      writeFileSync(join(dir, "screenshot.png"), Buffer.from(await this.session.screenshot(), "base64"));
      writeFileSync(join(dir, "page.txt"), await this.session.execute<string>("return document.body.innerText"));
      writeFileSync(join(dir, "terminals.json"), JSON.stringify(await this.terminalTexts(), null, 2));
    } catch (error) {
      writeFileSync(join(dir, "capture-error.txt"), String(error));
    }
    for (const log of ["daemon.out", "driver.log", "deck.log", join("state", "daemon.log")]) {
      try {
        writeFileSync(join(dir, log.replace("/", "-")), readFileSync(join(this.sandbox, log)));
      } catch {
        // Not every scenario produces every log.
      }
    }
    writeFileSync(join(dir, "sandbox.txt"), `${this.sandbox}\n`);
  }
}

/** Run `body` against a fresh deck, keeping evidence if it throws. */
export async function withDeck(name: string, options: DeckOptions, body: (deck: Deck) => Promise<void>): Promise<void> {
  const deck = await Deck.open(options, name);
  try {
    await body(deck);
  } catch (error) {
    await deck.close(error, name);
    throw error;
  }
  await deck.close();
}
