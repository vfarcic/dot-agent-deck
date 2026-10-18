// A stand-in Pi for the lane-1 tests of PRD #1666 (`inbox/pi/023`): it loads
// the real extension (`pi-extension/src/index.ts`, its receiver included) and
// plays Pi 0.84.4's event contract around it, with no model and no network.
//
//   node fake-pi.mjs <index.ts> <mode> <base>
//
// It writes its pid to `<base>.pid` and one line per hand-over it sees to
// `<base>.log` ("handed <id>", "handed repeat <id>", "committed <id>"). Modes:
//   never  — every hand-over passes the gate and never starts or commits;
//   silent — the first starts a turn and commits, and the turn never ends;
//   behind — the first starts a turn and commits; one handed during it waits
//            behind it and stays uncommitted after the turn ends at
//            FAKE_PI_TURN_MS;
//   slow   — each starts and commits FAKE_PI_START_MS after its hand-over.
import { execFile } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { pathToFileURL } from "node:url";

const [entry, mode, base] = process.argv.slice(2);
writeFileSync(`${base}.pid`, String(process.pid));
const log = (line) => appendFileSync(`${base}.log`, line + "\n");

// Pi resolves typebox from its own runtime; the tools only need a stand-in.
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "typebox") {
      return { url: "data:text/javascript,export const Type = new Proxy({}, {get:()=> (...args)=>({args})});", shortCircuit: true };
    }
    return next(specifier, context);
  },
});
const { default: extension } = await import(pathToFileURL(entry).href);

const handlers = new Map();
let run;
let queued = 0;
const ctx = {
  cwd: process.cwd(),
  get signal() {
    return run?.signal;
  },
  isIdle: () => run === undefined,
  hasPendingMessages: () => queued > 0,
  ui: {},
};
const emit = async (name, event = {}) => {
  let answer;
  for (const handler of handlers.get(name) ?? []) {
    const result = await handler(event, ctx);
    if (result !== undefined && answer === undefined) answer = result;
  }
  return answer;
};
const pi = {
  registerTool() {},
  on(name, handler) {
    handlers.set(name, [...(handlers.get(name) ?? []), handler]);
  },
  exec(bin, argv, options = {}) {
    return new Promise((resolve) => {
      execFile(bin, argv, { timeout: options.timeout, signal: options.signal }, (error, stdout, stderr) =>
        resolve({ code: error ? (typeof error.code === "number" ? error.code : 1) : 0, stdout: String(stdout), stderr: String(stderr), killed: false })
      );
    });
  },
  sendUserMessage(text) {
    void handOver(text);
  },
};
extension(pi);

const ids = (text) => [...text.matchAll(/\[dot-agent-deck message (m-[a-z2-7]{8}-[0-9a-f]{16})/g)].map((m) => m[1]);
let first = true;

async function handOver(text) {
  const answer = await emit("input", { text, source: "extension" });
  if (answer?.action !== "transform") return;
  const repeat = answer.text.includes("This may repeat dot-agent-deck message");
  for (const id of ids(answer.text)) log(`handed ${repeat ? "repeat " : ""}${id}`);
  const isFirst = first;
  first = false;
  if (mode === "never") return;
  if (mode === "slow") {
    setTimeout(() => void start(answer.text, true), Number(process.env.FAKE_PI_START_MS ?? 5000));
    return;
  }
  if (run !== undefined) {
    // Behind an active turn: a queued follow-up that this stand-in never runs.
    queued += 1;
    return;
  }
  if (isFirst) {
    await start(answer.text, false);
    if (mode === "behind") {
      setTimeout(() => void settle(), Number(process.env.FAKE_PI_TURN_MS ?? 6000));
    }
  }
}

async function start(prompt, end) {
  run = new AbortController();
  await emit("before_agent_start", { prompt });
  await emit("agent_start", {});
  await emit("message_start", { message: { role: "user", content: [{ type: "text", text: prompt }] } });
  for (const id of ids(prompt)) log(`committed ${id}`);
  if (end) await settle();
}

async function settle() {
  run = undefined;
  // `behind` leaves its follow-up queued: settling the running turn does not
  // mean Escape removed that follow-up. No replacement attempt should run.
  if (mode !== "behind") queued = 0;
  await emit("agent_settled", {});
}

await emit("session_start", { reason: "startup" });
process.stdout.write("FAKE-PI-READY\r\n");
// Pi's process lives until its terminal goes away.
process.stdin.resume();
process.stdin.on("end", () => process.exit(0));
