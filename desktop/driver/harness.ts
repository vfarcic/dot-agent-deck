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
import {
  accessSync,
  closeSync,
  constants,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { readFile } from "node:fs/promises";
import { connect, createServer, type Server, type Socket } from "node:net";
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

/**
 * Issue #1403 — how long a finished drag may take to show up as xterm's
 * selection. xterm makes the selection while it handles the drag, so this only
 * absorbs a slow runner, and a drag that selected nothing fails at once rather
 * than as a clipboard that never filled, `WAIT_MS` later.
 */
const SELECT_MS = 15_000;

/**
 * Issue #1403 — how long a terminal's grid must hold still after a click
 * before a drag on it is trusted. The resize it waits out (the window's focus
 * claim, see `selectRow`) landed within 10ms of the press on a GitHub runner,
 * so this is margin, not a measured bound.
 */
const SETTLE_MS = 1_000;

/**
 * How many drags `selectRow` makes when a resize of the grid during the drag
 * cleared what it selected. The focus claim that causes that resize happens
 * once per focus-in, so a second drag is the one that counts and a third is
 * margin.
 */
const DRAG_ATTEMPTS = 3;

/**
 * Issue #1457 — how long `setWindowFocus` waits for the page to agree with a
 * focus move before it makes the move again, and how many rounds of moves it
 * makes (one move per app window each round). A focus change reaches the page
 * in milliseconds; the bound is load margin.
 */
const FOCUS_MS = 10_000;
const FOCUS_ATTEMPTS = 3;

const paths = {
  app: process.env.DAD_DRIVER_APP ?? join(REPO_ROOT, "target", "debug", "dot-agent-deck-desktop"),
  daemon: process.env.DAD_DRIVER_DAEMON ?? join(REPO_ROOT, "target", "debug", "dot-agent-deck"),
  tauriDriver: process.env.DAD_DRIVER_TAURI_DRIVER ?? "tauri-driver",
  nativeDriver: process.env.DAD_DRIVER_NATIVE_DRIVER ?? "WebKitWebDriver",
  xclip: process.env.DAD_DRIVER_XCLIP ?? "xclip",
  xdotool: process.env.DAD_DRIVER_XDOTOOL ?? "xdotool",
  results: process.env.DAD_DRIVER_RESULTS ?? join(REPO_ROOT, "desktop", "driver-results"),
  // Issue #1356 — what the far deck's loopback `sshd` needs. Resolved when a
  // scenario asks for a far deck, so the others need none of it.
  sshd: process.env.DAD_DRIVER_SSHD,
  sshKeygen: process.env.DAD_DRIVER_SSH_KEYGEN ?? "ssh-keygen",
  nssWrapper: process.env.DAD_DRIVER_NSS_WRAPPER,
};

/**
 * Issue #1356 — where a system `sshd` is looked for: absolute paths, since it
 * is normally on no ordinary user's PATH. The list `tests/e2e_remote_deck_transport.rs`
 * uses, Linux's half of it.
 */
const SSHD_CANDIDATES = ["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/libexec/sshd", "/usr/local/sbin/sshd", "/run/current-system/sw/bin/sshd"];

/**
 * The `ssh` the app's tunnel runs: `SSH_PROGRAM_CANDIDATES` in
 * `src/remote_tunnel.rs`, first executable wins. The preflight runs the same
 * one, so a login it proves is the login the tunnel will make.
 */
const SSH_CANDIDATES = ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh", "/opt/homebrew/bin/ssh", "/opt/local/bin/ssh", "/run/current-system/sw/bin/ssh"];

/** Where `libnss_wrapper.so` is looked for: apt's `libnss-wrapper` and the usual multilib layouts. */
const NSS_WRAPPER_CANDIDATES = [
  "/usr/lib/x86_64-linux-gnu/libnss_wrapper.so",
  "/usr/lib/aarch64-linux-gnu/libnss_wrapper.so",
  "/usr/lib64/libnss_wrapper.so",
  "/usr/lib/libnss_wrapper.so",
];

/** The first of `candidates` this process could execute or read, or `undefined`. */
function firstAccessible(candidates: string[], mode: number): string | undefined {
  return candidates.find((candidate) => {
    try {
      accessSync(candidate, mode);
      return true;
    } catch {
      return false;
    }
  });
}

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

/**
 * Issue #1795 — `count` free ports, all different. Every listener stays bound
 * until the last port is chosen: a port is free again the moment its listener
 * closes, and Linux handed the same one to two back-to-back requests 3 times
 * in 20,000 (measured). tauri-driver given its own port as `--native-port`
 * forwards every request to itself, recursing until the ephemeral ports run
 * out, and the native driver cannot bind at all — the readiness wait then
 * spends its whole bound on a server that will never answer.
 */
export async function freePorts(count: number): Promise<number[]> {
  const servers: Server[] = [];
  try {
    for (let i = 0; i < count; i++) {
      const server = createServer();
      servers.push(server);
      await new Promise<void>((settle, fail) => {
        server.once("error", fail);
        server.listen(0, "127.0.0.1", settle);
      });
    }
    return servers.map((server) => {
      const address = server.address();
      if (typeof address !== "object" || !address) throw new Error("no port");
      return address.port;
    });
  } finally {
    await Promise.all(servers.map((server) => new Promise((settle) => server.close(settle))));
  }
}

/**
 * Issue #1795 — the line in `log` where the native driver said it could not
 * bind its port, if it did. tauri-driver stays up with nothing behind it, so
 * `assertRunning` sees no exit, and the line is in `driver.log` and nowhere
 * else. Read asynchronously because it runs on every readiness poll.
 */
async function nativeDriverBindFailure(log: string): Promise<string | undefined> {
  return (await readFile(log, "utf8"))
    .split("\n")
    .find((line) => line.includes("Unable to listen for HTTP server"))
    ?.trim();
}

function run(file: string, args: string[], env: NodeJS.ProcessEnv): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((settle) => {
    execFile(file, args, { env, timeout: WAIT_MS }, (error, stdout, stderr) => {
      const code = error ? (typeof error.code === "number" ? error.code : 1) : 0;
      settle({ code, stdout, stderr });
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

/** The attach protocol's frame kinds this harness speaks (`src/daemon_protocol.rs`). */
const KIND_REQ = 0x01;
const KIND_RESP = 0x02;

/** One attach-protocol frame: a kind byte, a big-endian u32 length, the payload. */
function frame(kind: number, payload: unknown): Buffer {
  const body = Buffer.from(JSON.stringify(payload));
  const header = Buffer.alloc(5);
  header[0] = kind;
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
}

type AttachResponse = { ok?: boolean; error?: string; applied_cols?: number; applied_rows?: number };

/**
 * Send one request on a fresh connection to the daemon's attach socket and
 * resolve with the socket once the daemon's response frame says ok. Anything
 * after the response — an attach stream's replay and live output — is read and
 * dropped, so the daemon never blocks writing to this client.
 */
function request(socketPath: string, payload: unknown, onResponse?: (response: AttachResponse) => void): Promise<Socket> {
  return new Promise((settle, fail) => {
    const socket = connect(socketPath);
    let buffered = Buffer.alloc(0);
    let answered = false;
    const timer = setTimeout(() => {
      socket.destroy();
      fail(new Error(`no response to ${JSON.stringify(payload)} within ${WAIT_MS}ms`));
    }, WAIT_MS);
    socket.once("error", (error) => {
      clearTimeout(timer);
      if (!answered) fail(error);
    });
    socket.once("connect", () => socket.write(frame(KIND_REQ, payload)));
    socket.on("data", (chunk: Buffer) => {
      if (answered) return;
      buffered = Buffer.concat([buffered, chunk]);
      if (buffered.length < 5) return;
      const length = buffered.readUInt32BE(1);
      if (buffered.length < 5 + length) return;
      answered = true;
      clearTimeout(timer);
      const response = JSON.parse(buffered.subarray(5, 5 + length).toString()) as AttachResponse;
      if (buffered[0] !== KIND_RESP || !response.ok) {
        socket.destroy();
        fail(new Error(`the daemon refused ${JSON.stringify(payload)}: ${JSON.stringify(response)}`));
        return;
      }
      onResponse?.(response);
      settle(socket);
    });
  });
}

/**
 * Issue #1356 — start `command` as an agent on the daemon behind `socketPath`,
 * the way the TUI does, over the attach protocol and not through any window.
 */
async function startAgent(socketPath: string, command: string, cwd: string, displayName: string): Promise<void> {
  (
    await request(socketPath, {
      op: "start-agent",
      command,
      cwd,
      rows: 24,
      cols: 80,
      env: [],
      display_name: displayName,
    })
  ).destroy();
}

type DaemonAgent = { agent_id: string; cwd?: string; label?: string };

/** A daemon's own view of its agents, from the CLI rather than the window. */
async function daemonAgents(env: NodeJS.ProcessEnv): Promise<DaemonAgent[]> {
  const { code, stdout } = await run(paths.daemon, ["daemon", "status", "--json"], env);
  if (code !== 0) throw new Error(`daemon status exited ${code}`);
  return (JSON.parse(stdout) as { agents: DaemonAgent[] }).agents;
}

/** What a scenario's deck is configured with. */
/** One mounted terminal, as the build-gated seam's `terminalScreens()` reports it. */
type TerminalScreen = {
  key: string;
  cols: number;
  rows: number;
  lines: string[];
  rect: { left: number; top: number; width: number; height: number } | null;
  selection: string;
};

export type DeckOptions = {
  /** Start the daemon before the window opens. `false` leaves it to `startDaemon()`. */
  daemonFirst: boolean;
  /** `default_command` in the deck's `config.toml`, which seeds New agent's Command field. */
  defaultCommand?: string;
  /** Explicit process overrides for a native-driver scenario (e.g. a system-browser recorder). */
  env?: NodeJS.ProcessEnv;
  /**
   * Issue #1356 — also start a second daemon, reached as a configured remote
   * deck through a loopback `sshd` this harness runs (see `FarDeck`), and open
   * the window with All Decks selected.
   */
  farDeck?: boolean;
};

export class Deck {
  readonly sandbox: string;
  /** A directory inside the sandbox, canonical, that the deck offers as its `default_dir`. */
  readonly project: string;
  readonly env: NodeJS.ProcessEnv;
  session!: Session;
  /** Issue #1356 — the second daemon, when the scenario asked for one. */
  far?: FarDeck;
  /** What the window runs under: `env`, plus what a far deck adds for the app's `ssh` alone. */
  private windowEnv?: NodeJS.ProcessEnv;
  private daemon?: ChildProcess;
  private driver?: ChildProcess;
  /** Issue #1457 — the app window whose focus this page follows, once `setWindowFocus` has found it. */
  private appWindow?: string;

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
      ...options.env,
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
      if (options.farDeck) {
        deck.far = await FarDeck.start(deck.sandbox, deck.env);
        deck.windowEnv = deck.far.windowEnv;
      }
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
    const [port, nativePort] = await freePorts(2);
    const driverLog = join(this.sandbox, "driver.log");
    // The app is launched by the native driver, which tauri-driver launches,
    // so this environment is the one the window runs under.
    this.driver = launch(
      paths.tauriDriver,
      ["--port", String(port), "--native-port", String(nativePort), "--native-driver", paths.nativeDriver],
      this.windowEnv ?? this.env,
      driverLog,
    );
    const base = `http://127.0.0.1:${port}`;
    // A bind failure is an answer, not a retry: `waitFor` retries a probe that
    // throws until its deadline, so the probe returns it and it is thrown here.
    const { bindFailure } = await waitFor("tauri-driver to accept sessions", async () => {
      assertRunning(this.driver, "tauri-driver");
      const failure = await nativeDriverBindFailure(driverLog);
      if (failure) return { bindFailure: failure };
      return (await serverReady(base)) && { bindFailure: undefined };
    });
    if (bindFailure) throw new Error(`the native driver could not bind its port: ${bindFailure}`);
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

  /**
   * Issue #1403 — two viewport points on the first visible row, of any mounted
   * terminal, whose text is exactly `text`: `from` in its first cell and `to`
   * at the screen's right edge, so a drag between them selects the whole row.
   * The drag runs to the edge rather than to the last character because
   * xterm rounds a pointer to a cell boundary — measured, a drag ending on the
   * last character's centre left that character out — and a selection past
   * the end of a row's text copies no trailing blanks. Read through the same
   * build-time seam as `terminalTexts`, since the WebGL renderer leaves no row
   * text in the DOM to measure instead.
   */
  async rowSpan(text: string): Promise<{ from: { x: number; y: number }; to: { x: number; y: number } } | undefined> {
    for (const { cols, rows, lines, rect } of await this.terminalScreens()) {
      const row = lines.findIndex((line) => line === text);
      if (row < 0 || !rect || cols < 1 || rows < 1) continue;
      const cell = { width: rect.width / cols, height: rect.height / rows };
      const y = rect.top + (row + 0.5) * cell.height;
      return {
        from: { x: rect.left + 0.5 * cell.width, y },
        to: { x: rect.left + rect.width - 1, y },
      };
    }
    return undefined;
  }

  /**
   * Issue #1403 — whether any mounted terminal's xterm holds exactly `text` as
   * its selection. Checked between a drag and the copy chord, so a failure
   * says which of the two did not happen.
   */
  async hasSelection(text: string): Promise<boolean> {
    return (await this.terminalScreens()).some(({ selection }) => selection === text);
  }

  /**
   * Issue #1403 — select the row whose text is exactly `text` the way a person
   * does: click into the terminal, then drag across the row, and wait for xterm
   * to hold `text` as its selection.
   *
   * The first click matters. WebDriver types into the page without the window
   * holding focus, so a press is also the window's focus-in, and on a focus-in
   * the app claims this terminal's size on its daemon (PRD #1105). When that
   * changes the grid's row count the rows move under the pointer: the terminal
   * carries the drag on from the cell it was pressed on (#1457, which
   * `terminal_003` covers), but this drag would still end where the row was
   * measured before the resize. The click lets that settle, and the row is
   * measured again after it.
   *
   * The click is not always the press that focuses the window. In five
   * failed runs on GitHub runners the grid held still for the whole settle
   * after the click and the resize landed 5-10ms after the DRAG's press
   * instead, so a drag that
   * ends with no selection after the grid resized during it is made again,
   * once the grid has settled at the new size and the row has been measured on
   * it, up to `DRAG_ATTEMPTS` drags. A drag that selected nothing with no
   * resize to explain it fails at once. Either failure carries the mouse events
   * the page saw during the last drag and every selection clear and resize,
   * with the stack that caused it.
   */
  async selectRow(text: string): Promise<void> {
    const before = await waitFor(`${text} on its own row`, () => this.rowSpan(text));
    await this.session.clickAt(before.from);
    // Installed once per page. The selection trace is never emptied.
    await this.traceTerminals();
    for (let attempt = 1; ; attempt += 1) {
      await this.gridSettled();
      const span = await waitFor(`${text} on its own row after the grid settled`, () => this.rowSpan(text));
      // Installed once per page; each drag only empties the pointer record,
      // and reads the page clock that a resize in the trace is stamped with.
      const draggedAt = await this.session.execute<number>(
        `window.__dadPointerLog = [];
         if (!window.__dadPointerProbe) {
           window.__dadPointerProbe = true;
           for (const type of ["mousedown", "mousemove", "mouseup"]) {
             document.addEventListener(type, (e) => {
               if (window.__dadPointerLog.length < 60) {
                 const mods = (e.shiftKey ? "S" : "") + (e.ctrlKey ? "C" : "") + (e.altKey ? "A" : "") + (e.metaKey ? "M" : "");
                 window.__dadPointerLog.push([type, Math.round(performance.now()), Math.round(e.clientX), Math.round(e.clientY), e.buttons, e.detail, mods]);
               }
             }, { capture: true });
           }
         }
         return Math.floor(performance.now());`,
      );
      await this.session.drag(span.from, span.to);
      let outcome: "selected" | "resized";
      try {
        outcome = await waitFor(
          `the drag to select ${text}`,
          async () => {
            if (await this.hasSelection(text)) return "selected" as const;
            if (await this.resizedSince(draggedAt)) return "resized" as const;
            return undefined;
          },
          SELECT_MS,
        );
      } catch (error) {
        throw new Error(`${(error as Error).message}; the page saw ${await this.selectionRecord()}`);
      }
      if (outcome === "selected") return;
      if (attempt >= DRAG_ATTEMPTS) {
        throw new Error(
          `the grid resized during each of ${attempt} drags to select ${text}; the page saw ${await this.selectionRecord()}`,
        );
      }
      console.warn(`selectRow: the grid resized during drag ${attempt} to select ${text}; dragging again once it settles`);
    }
  }

  /** Whether any mounted terminal resized at or after `since` on the page's clock. */
  private async resizedSince(since: number): Promise<boolean> {
    return this.session.execute<boolean>(
      `return (window.__dadSelectionTrace || []).some((entry) => entry[0] === "resize" && entry[1] >= ${since});`,
    );
  }

  /** The last drag's mouse events and the selection trace, for a failure message. */
  private async selectionRecord(): Promise<string> {
    const seen = await this.session
      .execute<unknown>("return { events: window.__dadPointerLog, trace: window.__dadSelectionTrace };")
      .catch((probe: unknown) => String(probe));
    return JSON.stringify(seen);
  }

  /**
   * Issue #1403 — start recording every selection clear and resize of the
   * mounted terminals, and the window's focus changes, into the record a failed
   * `selectRow` reports. Idempotent; call it as soon as a terminal is mounted
   * so the record covers the whole scenario.
   */
  async traceTerminals(): Promise<void> {
    await this.session.execute(
      `window.__dadSelectionTrace = window.__dadSelectionTrace || [];
       window.__dadDriver.traceSelection();
       if (!window.__dadFocusProbe) {
         window.__dadFocusProbe = true;
         for (const type of ["focus", "blur"]) {
           window.addEventListener(type, (e) => {
             if (e.target === window) window.__dadSelectionTrace.push(["window-" + type, Math.round(performance.now())]);
           });
         }
       }`,
    );
  }

  /** Until every mounted terminal's grid and box have held still for `SETTLE_MS`. */
  async gridSettled(): Promise<void> {
    let last = "";
    let since = Date.now();
    await waitFor("the terminal's grid to settle", async () => {
      const now = JSON.stringify((await this.terminalScreens()).map(({ cols, rows, rect }) => [cols, rows, rect]));
      if (now !== last) {
        last = now;
        since = Date.now();
      }
      return Date.now() - since >= SETTLE_MS;
    });
  }

  private async terminalScreens(): Promise<TerminalScreen[]> {
    const screens = await this.session.execute<TerminalScreen[] | null>(
      "return window.__dadDriver ? window.__dadDriver.terminalScreens() : null",
    );
    if (screens === null) {
      throw new Error("the bundle carries no driver seam: build it with VITE_DAD_DRIVER_SEAM=1 (docs/develop/desktop-gui.md)");
    }
    return screens;
  }

  /**
   * Issue #1403 — the SYSTEM clipboard's text, read by `xclip` from outside
   * the app, on the display the window is on. Not `navigator.clipboard` in the
   * page: that would ask the same webview whose clipboard path is under test,
   * and a read there needs a permission this app never grants. `null` when
   * nothing owns the clipboard yet, which `xclip` reports as a failure.
   */
  async clipboardText(): Promise<string | null> {
    const { code, stdout } = await run(paths.xclip, ["-selection", "clipboard", "-o"], this.env);
    return code === 0 ? stdout : null;
  }

  /** The daemon's own view of its agents, from the CLI rather than the window. */
  async daemonAgents(): Promise<DaemonAgent[]> {
    return daemonAgents(this.env);
  }

  /** Issue #1356 — start `command` in `cwd` on this deck's daemon over its attach socket. */
  async startAgent(command: string, cwd: string, displayName: string): Promise<void> {
    await startAgent(this.env.DOT_AGENT_DECK_ATTACH_SOCKET as string, command, cwd, displayName);
  }

  /** Issue #1457 — what every mounted terminal's xterm holds as its selection, for a failure message. */
  async selections(): Promise<string[]> {
    return (await this.terminalScreens()).map(({ selection }) => selection);
  }

  /** Issue #1457 — every mounted terminal's grid as `[cols, rows]`. */
  async grids(): Promise<[number, number][]> {
    return (await this.terminalScreens()).map(({ cols, rows }) => [cols, rows]);
  }

  /** Read the daemon's applied grid without contributing a viewport or claiming focus. */
  async appliedGrid(agentId: string): Promise<[number, number]> {
    let applied: AttachResponse = {};
    const stream = await request(this.env.DOT_AGENT_DECK_ATTACH_SOCKET as string, {
      op: "attach-stream",
      id: agentId,
      // With neither rows/cols nor geometry_updates, this subscribes without
      // registering a viewer constraint (AgentPtyRegistry's legacy observer).
    }, (response) => { applied = response; });
    stream.destroy();
    if (!applied.applied_cols || !applied.applied_rows) {
      throw new Error(`the daemon reported no applied grid for ${agentId}: ${JSON.stringify(applied)}`);
    }
    return [applied.applied_cols, applied.applied_rows];
  }

  /**
   * Issue #1457 — a second client on the same agent, standing in for a TUI the
   * person was using before they came to the window: it attaches to `agentId`
   * as a viewer of `rows` x `cols` and claims focus, so under PRD #1105's
   * last-focused-wins rule the daemon sizes the agent to it, and the window's
   * grid with it. It speaks the attach protocol directly, as the TUI does,
   * through the sandbox's own socket. `close()` detaches it.
   */
  async standInClient(agentId: string, rows: number, cols: number): Promise<{ close: () => void }> {
    const socketPath = this.env.DOT_AGENT_DECK_ATTACH_SOCKET as string;
    const clientId = "c-driver-stand-in-tui";
    const stream = await request(socketPath, {
      op: "attach-stream",
      id: agentId,
      rows,
      cols,
      geometry_updates: true,
      client_id: clientId,
    });
    stream.on("data", () => undefined);
    stream.on("error", () => undefined);
    try {
      (await request(socketPath, { op: "focus-gained", client_id: clientId })).destroy();
    } catch (error) {
      // A viewer left attached would keep sizing the agent for the rest of the scenario.
      stream.destroy();
      throw error;
    }
    return { close: () => stream.destroy() };
  }

  /**
   * Issue #1457 — move the display's input focus, as a window manager does: to
   * the app's window, or away from it to the root window, which is the
   * window-manager-free way to put "another window in front". xvfb runs no
   * window manager, so a press never moves focus by itself, and this is how a
   * scenario makes a press also be the window's focus-in.
   *
   * Focus on the root window follows the pointer into whatever window is under
   * it, so the display's real pointer is parked outside the app's window first
   * — measured: without that, the app kept its focus. WebDriver's pointer
   * actions are the webview's own, and never move that pointer.
   */
  async setWindowFocus(focused: boolean): Promise<void> {
    const xdotool = async (...args: string[]) => {
      const { code, stdout } = await run(paths.xdotool, args, this.env);
      if (code !== 0) throw new Error(`xdotool ${args.join(" ")} exited ${code}`);
      return stdout.trim().split("\n").filter(Boolean);
    };
    // More than one window can carry the app's title. Measured locally, in
    // runs of the whole file: two, owned by different X clients, and which of
    // them the page's focus follows varied — even within one scenario, the one
    // that worked last time once did not. Why is not established. So each is
    // tried in turn, the one that worked last time first.
    const found = await xdotool("search", "--onlyvisible", "--name", "Agent Deck");
    const apps = found.includes(this.appWindow ?? "")
      ? [this.appWindow as string, ...found.filter((window) => window !== this.appWindow)]
      : found;
    const [root] = await xdotool("search", "--maxdepth", "0", "");
    if (apps.length === 0 || !root) throw new Error(`xdotool found no ${apps.length ? "root" : "app"} window`);
    const parkPointer = async () => {
      const rects = await Promise.all(
        apps.map(async (window) => {
          const g = Object.fromEntries(
            (await xdotool("getwindowgeometry", "--shell", window)).map((line) => line.split("=")),
          ) as Record<string, string>;
          return { x: Number(g.X), y: Number(g.Y), right: Number(g.X) + Number(g.WIDTH), below: Number(g.Y) + Number(g.HEIGHT) };
        }),
      );
      const [width, height] = (await xdotool("getdisplaygeometry"))[0].split(" ").map(Number);
      const outside = [
        [width - 1, height - 1],
        [0, height - 1],
        [width - 1, 0],
        [0, 0],
      ].find(([x, y]) => rects.every((r) => x < r.x || x >= r.right || y < r.y || y >= r.below));
      if (!outside) throw new Error(`the app's windows cover every corner of the ${width}x${height} display`);
      await xdotool("mousemove", "--sync", String(outside[0]), String(outside[1]));
    };
    const reported = async () => (await this.session.execute<boolean>("return document.hasFocus()")) === focused;
    if (!focused) await parkPointer();
    const attempts = focused ? FOCUS_ATTEMPTS * apps.length : FOCUS_ATTEMPTS;
    for (let attempt = 1; ; attempt += 1) {
      const target = focused ? apps[(attempt - 1) % apps.length] : root;
      // A retry leaves through the root window first, so the move is a change
      // of focus even when X already had it on `target`: X sends no focus
      // event for a move that changes nothing.
      if (attempt > 1 && focused) {
        await parkPointer();
        await xdotool("windowfocus", "--sync", root);
      }
      await xdotool("windowfocus", "--sync", target);
      try {
        await waitFor(`the page to report ${focused ? "having" : "losing"} focus`, reported, FOCUS_MS);
        if (focused) this.appWindow = target;
        return;
      } catch (error) {
        if (attempt < attempts) {
          console.warn(`setWindowFocus: the page did not report ${focused ? "having" : "losing"} focus after focusing ${target}; trying again`);
          continue;
        }
        const holder = await xdotool("getwindowfocus").catch(() => ["?"]);
        throw new Error(
          `${(error as Error).message}, ${attempt} times; X focus is on ${holder.join(",")}, ` +
            `app window(s) ${apps.join(",")}, root ${root}`,
        );
      }
    }
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
    await this.far?.close();
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
      writeFileSync(join(dir, "terminal-screens.json"), JSON.stringify(await this.terminalScreens(), null, 2));
    } catch (error) {
      writeFileSync(join(dir, "capture-error.txt"), String(error));
    }
    // Every scenario's far-deck paths, not only those whose far deck came up:
    // a far deck that failed to start has the logs that say why, and a
    // scenario without one has none of these files to copy.
    for (const log of ["daemon.out", "driver.log", "deck.log", join("state", "daemon.log"), ...FarDeck.LOGS]) {
      try {
        writeFileSync(join(dir, log.replaceAll("/", "-")), readFileSync(join(this.sandbox, log)));
      } catch {
        // Not every scenario produces every log.
      }
    }
    writeFileSync(join(dir, "sandbox.txt"), `${this.sandbox}\n`);
  }
}

/**
 * Issue #1356 — a second real daemon that the window reaches the way it
 * reaches every deck but its local one: a row in the deck list
 * (`remotes.toml`), carried by the app's own `ssh -N -L` child to the far
 * daemon's attach socket. The far end is an `sshd` this harness runs: its own
 * host key, its own `authorized_keys` naming a keypair generated here, on an
 * ephemeral loopback port, as the current user — the approach
 * `tests/e2e_remote_deck_transport.rs` (`fleet/observe/002`) takes.
 *
 * What that file could do and this one cannot is hand the tunnel a different
 * `ssh`: the app resolves its own from `SSH_PROGRAM_CANDIDATES`, and its
 * forced `StrictHostKeyChecking=yes` is anchored at `~/.ssh/known_hosts` —
 * which OpenSSH expands against `getpwuid()`'s home, not `$HOME`, so the
 * sandbox `HOME` does not move it. Writing the generated host key into the
 * operator's real `~/.ssh` is not a trade a test may make. So the window runs
 * under `nss_wrapper` (cwrap's `LD_PRELOAD` library, made for exactly this),
 * whose passwd file gives this uid the sandbox home: the app's `ssh` finds the
 * one `known_hosts` line written here, and strict checking still runs — the
 * tunnel would not come up if the key did not match. Measured both ways
 * before relying on it: the same `ssh -o BatchMode=yes -o
 * StrictHostKeyChecking=yes` login failed with "No ED25519 host key is known"
 * without the wrapper and succeeded with it.
 *
 * Only the window gets the wrapper (`windowEnv`), with `XDG_RUNTIME_DIR` moved
 * into the sandbox, because the tunnel's start-up sweep deletes stale sockets
 * under `$XDG_RUNTIME_DIR/dot-agent-deck/tunnels` and must not reach the
 * operator's. `SSH_AUTH_SOCK` is dropped, so the generated key is the only one
 * offered. The daemons, the `sshd` and every CLI call run without the wrapper.
 */
export class FarDeck {
  /** The far deck's name in the deck list, which the window shows as its group's name. */
  static readonly NAME = "far-deck";
  /** The far side's logs, relative to the sandbox, for a failure's evidence. */
  static readonly LOGS = [
    join("far", "daemon.out"),
    join("far", "deck.log"),
    join("far", "state", "daemon.log"),
    join("ssh", "sshd.log"),
    join("ssh", "sshd.out"),
  ];

  /** The far daemon's own environment: every socket, state dir and home its own. */
  readonly env: NodeJS.ProcessEnv;
  /** A directory the far deck's agent runs in, distinct from the local deck's. */
  readonly project: string;
  /** What the window runs under — see the class comment. Set once the far end is up. */
  windowEnv!: NodeJS.ProcessEnv;
  private readonly sandbox: string;
  /** The window's deck's environment, which the far deck's own is derived from. */
  private readonly deckEnv: NodeJS.ProcessEnv;
  private readonly tools: { sshd: string; ssh: string; nssWrapper: string };
  private readonly far: string;
  private readonly ssh: string;
  private daemon?: ChildProcess;
  private sshd?: ChildProcess;

  private constructor(sandbox: string, deckEnv: NodeJS.ProcessEnv, tools: { sshd: string; ssh: string; nssWrapper: string }) {
    this.sandbox = sandbox;
    this.deckEnv = deckEnv;
    this.tools = tools;
    this.far = join(sandbox, "far");
    this.ssh = join(sandbox, "ssh");
    this.project = join(this.far, "far-project");
    mkdirSync(this.project, { recursive: true });
    mkdirSync(join(this.far, "home"));
    mkdirSync(join(this.far, "state"));
    mkdirSync(this.ssh, { mode: 0o700 });
    writeFileSync(join(this.far, "config.toml"), `default_dir = ${JSON.stringify(this.project)}\n`);
    const home = join(this.far, "home");
    this.env = {
      ...deckEnv,
      HOME: home,
      XDG_CONFIG_HOME: join(home, ".config"),
      XDG_CACHE_HOME: join(home, ".cache"),
      XDG_DATA_HOME: join(home, ".local", "share"),
      XDG_STATE_HOME: join(home, ".local", "state"),
      DOT_AGENT_DECK_ATTACH_SOCKET: join(this.far, "attach.sock"),
      DOT_AGENT_DECK_SOCKET: join(this.far, "hook.sock"),
      DOT_AGENT_DECK_STATE_DIR: join(this.far, "state"),
      DOT_AGENT_DECK_SESSION: join(this.far, "session.toml"),
      DOT_AGENT_DECK_SCHEDULES: join(this.far, "schedules.toml"),
      DOT_AGENT_DECK_LOG: join(this.far, "deck.log"),
      DOT_AGENT_DECK_DESKTOP_CONFIG: join(this.far, "desktop.toml"),
      DOT_AGENT_DECK_CONFIG: join(this.far, "config.toml"),
    };
  }

  /**
   * Start the far daemon and its `sshd`, register it in the window's deck list
   * with All Decks selected, and prove the login the tunnel will make before
   * any window depends on it. Everything it started is stopped if it throws.
   */
  static async start(sandbox: string, deckEnv: NodeJS.ProcessEnv): Promise<FarDeck> {
    const sshd = paths.sshd ?? firstAccessible(SSHD_CANDIDATES, constants.X_OK);
    const ssh = firstAccessible(SSH_CANDIDATES, constants.X_OK);
    const nssWrapper = paths.nssWrapper ?? firstAccessible(NSS_WRAPPER_CANDIDATES, constants.R_OK);
    if (!sshd) throw new Error(`no sshd at ${SSHD_CANDIDATES.join(", ")} (apt: openssh-server, or set DAD_DRIVER_SSHD)`);
    if (!ssh) throw new Error(`no ssh at ${SSH_CANDIDATES.join(", ")}, where the app's tunnel looks for one`);
    if (!nssWrapper) {
      throw new Error(
        `no libnss_wrapper.so at ${NSS_WRAPPER_CANDIDATES.join(", ")} (apt: libnss-wrapper, or set ` +
          "DAD_DRIVER_NSS_WRAPPER; docs/develop/desktop-gui.md's driver tier says how to get one without root)",
      );
    }
    const far = new FarDeck(sandbox, deckEnv, { sshd, ssh, nssWrapper });
    try {
      await far.startDaemon();
      const { port, clientKey } = await far.startSshd();
      await far.configureWindow(port, clientKey);
      await far.proveLogin(port, clientKey);
    } catch (error) {
      await far.close();
      throw error;
    }
    return far;
  }

  /** Start the far `daemon serve` and wait until it answers a Hello, as `Deck.startDaemon` does. */
  private async startDaemon(): Promise<void> {
    this.daemon = launch(paths.daemon, ["daemon", "serve"], this.env, join(this.far, "daemon.out"));
    await waitFor("the far daemon to answer a Hello", async () => {
      assertRunning(this.daemon, "the far daemon");
      return (await run(paths.daemon, ["daemon", "endpoint"], this.env)).code === 0;
    });
  }

  /**
   * Generate the keys, start `sshd` on a free loopback port and wait for its
   * own log to say it is listening. That wait is on a line rather than a
   * probe connection, because OpenSSH 9.8+ penalises a source address for a
   * connection that never authenticates, and a readiness probe would be one.
   * A lost race for the port is retried on another port, a few times.
   */
  private async startSshd(): Promise<{ port: number; clientKey: string }> {
    const hostKey = join(this.ssh, "hostkey");
    const clientKey = join(this.ssh, "clientkey");
    for (const key of [hostKey, clientKey]) {
      const { code, stderr } = await run(paths.sshKeygen, ["-q", "-t", "ed25519", "-N", "", "-C", "dad-driver", "-f", key], this.deckEnv);
      if (code !== 0) throw new Error(`${paths.sshKeygen} exited ${code} generating ${key}: ${stderr.trim()}`);
    }
    const authorizedKeys = join(this.ssh, "authorized_keys");
    writeFileSync(authorizedKeys, readFileSync(`${clientKey}.pub`), { mode: 0o600 });
    const log = join(this.ssh, "sshd.log");
    let last = "";
    for (let attempt = 1; attempt <= 3; attempt += 1) {
      const [port] = await freePorts(1);
      const config = join(this.ssh, "sshd_config");
      // A private sshd: its own key material and port, no PAM, no password
      // login, nothing the tunnel does not need. `StrictModes no` because the
      // key files are in the sandbox rather than under a `~/.ssh`;
      // `AllowStreamLocalForwarding` because the tunnel forwards a Unix socket.
      writeFileSync(
        config,
        [
          `Port ${port}`,
          "ListenAddress 127.0.0.1",
          `HostKey ${hostKey}`,
          `AuthorizedKeysFile ${authorizedKeys}`,
          "PidFile none",
          "StrictModes no",
          "UsePAM no",
          "PasswordAuthentication no",
          "KbdInteractiveAuthentication no",
          "PubkeyAuthentication yes",
          // `yes`, though the tunnel forwards no TCP: measured, OpenSSH 10.2
          // denied the Unix-socket `-L` ("open failed") with this `no`, even
          // beside `AllowStreamLocalForwarding yes`.
          "AllowTcpForwarding yes",
          "AllowStreamLocalForwarding yes",
          "X11Forwarding no",
          "PrintMotd no",
          "LogLevel VERBOSE",
          "",
        ].join("\n"),
      );
      writeFileSync(log, "");
      // Its own process group, so teardown reaches the per-connection
      // children it forks with one signal to this group and no other.
      const fd = openSync(join(this.ssh, "sshd.out"), "a");
      const sshd = spawn(this.tools.sshd, ["-D", "-f", config, "-E", log], { env: this.deckEnv, stdio: ["ignore", fd, fd], detached: true });
      closeSync(fd);
      sshd.once("error", (error) => {
        (sshd as ChildProcess & { launchError?: Error }).launchError = error;
      });
      this.sshd = sshd;
      const listening = `Server listening on 127.0.0.1 port ${port}`;
      const up = await waitFor("the far deck's sshd to listen", async () => {
        if ((await readFile(log, "utf8")).includes(listening)) return "listening";
        try {
          assertRunning(sshd, "sshd");
        } catch (error) {
          return (error as Error).message;
        }
        return undefined;
      });
      if (up === "listening") return { port, clientKey };
      last = `attempt ${attempt}: ${up}; sshd said: ${readFileSync(log, "utf8").trim()}`;
      await this.stopSshd();
    }
    throw new Error(`the far deck's sshd would not come up: ${last}`);
  }

  /**
   * Write what the window reads: the host key's one `known_hosts` line in the
   * window's sandbox home, the passwd and group files that make that home
   * `getpwuid()`'s answer, the deck list naming the far daemon, and
   * `desktop.toml` selecting All Decks.
   */
  private async configureWindow(port: number, clientKey: string): Promise<void> {
    const home = this.deckEnv.HOME as string;
    mkdirSync(join(home, ".ssh"), { mode: 0o700 });
    // From the generated key rather than `ssh-keyscan`, which would ask the
    // server for its key and then believe the answer. `[host]:port` is how
    // OpenSSH keys a non-default port.
    const [algorithm, material] = readFileSync(join(this.ssh, "hostkey.pub"), "utf8").trim().split(/\s+/);
    writeFileSync(join(home, ".ssh", "known_hosts"), `[127.0.0.1]:${port} ${algorithm} ${material}\n`, { mode: 0o600 });

    const lookup = async (database: string, key: number) => {
      const { code, stdout } = await run("getent", [database, String(key)], this.deckEnv);
      if (code !== 0) throw new Error(`getent ${database} ${key} exited ${code}`);
      return stdout.trim();
    };
    const uid = process.getuid?.() ?? 0;
    const gid = process.getgid?.() ?? 0;
    const user = (await lookup("passwd", uid)).split(":");
    user[5] = home;
    const passwd = [user.join(":")];
    const group = [await lookup("group", gid)];
    if (uid !== 0) passwd.push(await lookup("passwd", 0));
    if (gid !== 0) group.push(await lookup("group", 0));
    const nss = join(this.sandbox, "nss");
    mkdirSync(nss);
    writeFileSync(join(nss, "passwd"), `${passwd.join("\n")}\n`);
    writeFileSync(join(nss, "group"), `${group.join("\n")}\n`);

    const version = (await run(paths.daemon, ["--version"], this.deckEnv)).stdout.trim().split(/\s+/).pop() ?? "0.0.0";
    const remotes = join(this.sandbox, "remotes.toml");
    writeFileSync(
      remotes,
      [
        "[[remotes]]",
        `name = ${JSON.stringify(FarDeck.NAME)}`,
        'type = "ssh"',
        'host = "127.0.0.1"',
        `port = ${port}`,
        `key = ${JSON.stringify(clientKey)}`,
        `socket = ${JSON.stringify(this.env.DOT_AGENT_DECK_ATTACH_SOCKET)}`,
        `version = ${JSON.stringify(version)}`,
        'added_at = "2026-01-01T00:00:00Z"',
        "",
      ].join("\n"),
    );
    writeFileSync(this.deckEnv.DOT_AGENT_DECK_DESKTOP_CONFIG as string, 'version = 1\n\n[endpoints]\nselection = "all"\n');

    const runtime = join(this.sandbox, "run");
    mkdirSync(runtime, { mode: 0o700 });
    const preload = [this.tools.nssWrapper, this.deckEnv.LD_PRELOAD].filter(Boolean).join(":");
    const { SSH_AUTH_SOCK: _agent, ...withoutAgent } = this.deckEnv;
    this.windowEnv = {
      ...withoutAgent,
      LD_PRELOAD: preload,
      NSS_WRAPPER_PASSWD: join(nss, "passwd"),
      NSS_WRAPPER_GROUP: join(nss, "group"),
      XDG_RUNTIME_DIR: runtime,
      DOT_AGENT_DECK_REMOTES: remotes,
    };
  }

  /**
   * The login the tunnel will make, made first by the same `ssh` under the
   * window's environment, so a host that cannot run this is a named failure
   * here rather than a deck that never connects, `WAIT_MS` later.
   */
  private async proveLogin(port: number, clientKey: string): Promise<void> {
    const args = ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=10"];
    const { code, stderr } = await run(this.tools.ssh, [...args, "-p", String(port), "-i", clientKey, "--", "127.0.0.1", "true"], this.windowEnv);
    if (code !== 0) {
      // ssh's own stderr carries the reason this check exists for (an unknown
      // host key, a preload that did not load); the sshd log, the server's half.
      throw new Error(
        `a pubkey login to the far deck's sshd with strict host-key checking failed (exit ${code}); ` +
          `ssh said: ${stderr.trim()}; sshd said: ${readFileSync(join(this.ssh, "sshd.log"), "utf8").trim()}`,
      );
    }
  }

  /** The far daemon's own view of its agents, from the CLI. */
  async daemonAgents(): Promise<DaemonAgent[]> {
    return daemonAgents(this.env);
  }

  /** Start `command` in `cwd` on the far daemon over its attach socket. */
  async startAgent(command: string, cwd: string, displayName: string): Promise<void> {
    await startAgent(this.env.DOT_AGENT_DECK_ATTACH_SOCKET as string, command, cwd, displayName);
  }

  /** Stop the far daemon alone: peer-pid-scoped `daemon stop`, then its own child by pid. */
  async stopDaemon(): Promise<void> {
    if (!this.daemon) return;
    await run(paths.daemon, ["daemon", "stop", "--force"], this.env);
    await stopChild(this.daemon);
    this.daemon = undefined;
  }

  /** Stop `sshd` and every connection child it forked: its own process group, and no other. */
  private async stopSshd(): Promise<void> {
    const sshd = this.sshd;
    this.sshd = undefined;
    if (!sshd?.pid) return;
    const signalGroup = (signal: NodeJS.Signals) => {
      try {
        process.kill(-(sshd.pid as number), signal);
      } catch {
        // ESRCH: the group is already gone.
      }
    };
    signalGroup("SIGTERM");
    await stopChild(sshd);
    signalGroup("SIGKILL");
  }

  /**
   * The app's own `ssh` tunnel children. The app starts each in its own
   * session, so a window that exits without closing its tunnels leaves them
   * running — measured on the first run of `fleet_001`. Matched on the
   * sandbox's own tunnel directory, which nothing outside this sandbox names.
   */
  private reapTunnels(): void {
    const marker = `${join(this.sandbox, "run", "dot-agent-deck", "tunnels")}/`;
    for (const entry of readdirSync("/proc")) {
      if (!/^\d+$/.test(entry)) continue;
      try {
        if (readFileSync(join("/proc", entry, "cmdline"), "utf8").includes(marker)) process.kill(Number(entry), "SIGKILL");
      } catch {
        // Gone already, or not ours to read.
      }
    }
  }

  async close(): Promise<void> {
    this.reapTunnels();
    await this.stopDaemon();
    await this.stopSshd();
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
