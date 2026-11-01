// A fake OpenCode 2.x host around the REAL installed deck plugins (PRD #2050):
// the dual-shape server module's `setup(ctx)` and the 2.x TUI half's
// `setup(api)`, both loaded in this one process. No OpenCode, model or
// credential. Composer calls are recorded as violations.
//
// The shapes follow OpenCode 2.2.5's own declarations (`@opencode/plugin@2.2.5`
// `dist/promise/plugin.d.ts` `Context`, `dist/tui/context.d.ts` `Context`;
// `@opencode/client@2.2.5` `dist/promise/generated/{client,types}.d.ts`):
// events are `{ id, created, type, location, data }` and arrive through
// `ctx.event.subscribe({ types })` (an async iterable) on the server side and
// `api.data.on(type, fn)` / `api.data.listen(fn)` on the TUI side; prompts go
// through `session.prompt({ sessionID, text, delivery? })`, which resolves to
// the queued `SessionInboxUser`. The server context and the TUI's `api.client`
// share one session API, so a prompt is recorded the same whichever half sends
// it. Anything this host does not implement answers with a recorder that logs
// `unimplemented` with its path, so a RED run says which member was missing.
import fs from 'node:fs';
import cp from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { pathToFileURL } from 'node:url';
import { performance } from 'node:perf_hooks';

const [serverPath, tuiPath, base, scenario] = process.argv.slice(2);
const config = JSON.parse(fs.readFileSync(base + '.config', 'utf8'));
const log = (kind, data = {}) => fs.appendFileSync(base + '.events', JSON.stringify({kind, t: Date.now(), monotonicMs: Math.trunc(performance.now()), ...data}) + '\n');
process.on('uncaughtException', (error) => { log('host-error', {message: String(error?.stack ?? error)}); });
process.on('unhandledRejection', (error) => { log('host-error', {message: String(error?.stack ?? error)}); });

// The plugin's watcher children, recorded like the 1.x host's.
const spawned = [];
const realSpawn = cp.spawn;
cp.spawn = (...args) => {
  const child = realSpawn(...args);
  spawned.push(child);
  log('spawn', {pid: child.pid, args: args.slice(0, 2)});
  const write = child.stdin?.write.bind(child.stdin);
  if (write) child.stdin.write = (bytes, ...rest) => { log('stdin', {text: bytes.toString()}); return write(bytes, ...rest); };
  child.stdout?.on?.('data', (bytes) => log('stdout', {pid: child.pid, text: bytes.toString()}));
  child.on('exit', (code, signal) => log('exit', {pid: child.pid, code, signal}));
  return child;
};
syncBuiltinESMExports();
if (process.stdin.isTTY) process.stdin.setRawMode(true);
process.stdin.on('data', chunk => fs.appendFileSync(base + '.bytes', chunk));
// A standalone 2.x client's command line (`opencode --standalone`).
process.argv = ['node', 'opencode', ...(config.argv ?? ['--standalone'])];

const directory = process.cwd();
const location = {directory};
let seq = 0;
const now = () => Date.now();
const sessions = new Map();
const status = new Map();
const addSession = (id, extra = {}) => {
  const info = {id, projectID: 'fixture-project', cost: 0, tokens: {input: 0, output: 0, reasoning: 0, cache: {read: 0, write: 0}}, time: {created: now(), updated: now()}, ...extra};
  sessions.set(id, info); status.set(id, 'idle'); return info;
};
addSession('shown');

// One event bus feeding both halves.
const iterators = new Set();
const tuiHandlers = new Map();
const tuiListeners = new Set();
const publish = (type, data) => {
  const event = {id: 'evt-' + (++seq), created: now(), type, location, data};
  if (type === 'session.status') status.set(data.sessionID, data.status.type === 'idle' ? 'idle' : 'running');
  for (const it of iterators) it.push(event);
  for (const fn of tuiHandlers.get(type) ?? []) { try { fn(event); } catch (e) { log('host-error', {message: String(e)}); } }
  for (const fn of tuiListeners) { try { fn({details: event}); } catch (e) { log('host-error', {message: String(e)}); } }
  log('emitted', {type, data});
  return event;
};
const subscribe = (options = {}) => {
  const types = options.types ? new Set(options.types) : null;
  const queue = []; let wake = null; let done = false;
  const it = {push(event) { if (types && !types.has(event.type)) return; queue.push(event); wake?.(); }};
  iterators.add(it);
  options.signal?.addEventListener?.('abort', () => { done = true; iterators.delete(it); wake?.(); });
  return {[Symbol.asyncIterator]() { return {
    async next() {
      while (!queue.length && !done) await new Promise(resolve => { wake = resolve; });
      wake = null;
      return done && !queue.length ? {done: true, value: undefined} : {done: false, value: queue.shift()};
    },
    async return() { done = true; iterators.delete(it); return {done: true, value: undefined}; },
  }; }};
};

// Recorder for every member this host does not implement.
const stub = (path) => new Proxy(function () {}, {
  get(_, prop) { if (prop === 'then' || typeof prop === 'symbol') return undefined; return stub(path + '.' + String(prop)); },
  apply(_, __, args) { log('unimplemented', {path, args: safe(args)}); return undefined; },
});
const safe = (value) => { try { return JSON.parse(JSON.stringify(value ?? null)); } catch { return String(value); } };
const lenient = (target, path) => new Proxy(target, {
  get(obj, prop) {
    if (prop in obj) return obj[prop];
    if (prop === 'then' || typeof prop === 'symbol') return undefined;
    return stub(path + '.' + String(prop));
  },
});

// The session API both halves share.
let inbox = 0;
const hooks = new Map();
const registration = (map, name, fn) => {
  if (!map.has(name)) map.set(name, []);
  map.get(name).push(fn);
  log('hook', {name});
  return {dispose: async () => { const list = map.get(name) ?? []; const i = list.indexOf(fn); if (i >= 0) list.splice(i, 1); }};
};
const prompt = (via) => async (input) => {
  const sessionID = input?.sessionID;
  log('prompt', {via, request: safe(input)});
  if (config.reject) throw new Error('fixture rejects');
  if (!sessions.has(sessionID)) throw new Error(`Session not found: ${sessionID}`);
  const inboxID = 'inbox-' + (++inbox);
  const delivery = input.delivery ?? 'queue';
  // The server's prompt hook sees the prompt before it is queued, and may
  // rewrite its text (`SessionPrompt.prompt` is mutable).
  const hooked = {sessionID, messageID: inboxID, prompt: {text: input.text}, delivery};
  for (const fn of hooks.get('prompt') ?? []) await fn(hooked);
  const item = {id: inboxID, sessionID, time: {created: now()}, type: 'user', payload: {text: hooked.prompt.text, files: input.files ?? []}, delivery};
  setImmediate(() => {
    publish('session.inbox.enqueued', {sessionID, inboxID, item});
    publish('session.inbox.delivered', {sessionID, inboxID});
    publish('session.execution.started', {sessionID});
    publish('session.status', {sessionID, status: {type: 'busy'}});
    log('user-message', {id: inboxID, sessionID});
  });
  return item;
};
const sessionApi = (via) => ({
  list: async () => ({data: [...sessions.values()], cursor: {}}),
  get: async ({sessionID}) => { const info = sessions.get(sessionID); if (!info) throw new Error(`Session not found: ${sessionID}`); return info; },
  create: async (input = {}) => { const id = 'created-' + (sessions.size); const info = addSession(id, input.parentID ? {parentID: input.parentID} : {}); log('create', {id, via}); publish('session.created', {sessionID: id, projectID: 'fixture-project', location, slug: id}); return info; },
  context: async ({sessionID}) => (config.messages?.[sessionID] ?? []),
  prompt: prompt(via),
  synthetic: async (input) => { log('synthetic', {via, request: safe(input)}); return {id: 'synthetic-' + (++inbox), sessionID: input?.sessionID, type: 'synthetic'}; },
  interrupt: async (input) => { log('interrupt', {via, request: safe(input)}); },
  update: async (input) => { log('session-update', {via, request: safe(input)}); },
  switchModel: async (input) => { log('switch-model', {via, request: safe(input)}); },
  switchAgent: async (input) => { log('switch-agent', {via, request: safe(input)}); },
  inbox: {list: async () => [], cancel: async (input) => log('inbox-cancel', {via, request: safe(input)}), update: async () => {}},
  wait: async () => {},
  instructions: async () => [],
  hook: async (name, fn) => registration(hooks, name, fn),
});
const toolHooks = new Map();
const permissionReply = async (input) => { log('permission-reply', {request: safe(input)}); };
const composer = new Proxy({}, {get(_, name) { if (name === 'then' || typeof name === 'symbol') return undefined; return (...args) => { log('COMPOSER-TOUCHED', {name: String(name), args: safe(args)}); }; }});

// OpenCode 2.x server-plugin context (`@opencode/plugin` `Context`).
const ctx = lenient({
  location,
  options: {},
  app: lenient({version: 'fixture-2.2.5'}, 'app'),
  event: {subscribe},
  session: lenient(sessionApi('server'), 'session'),
  tool: lenient({hook: async (name, fn) => registration(toolHooks, name, fn)}, 'tool'),
  permission: lenient({reply: permissionReply}, 'permission'),
}, 'ctx');

// OpenCode 2.x TUI-plugin context (`@opencode/plugin/tui` `Context`).
let route = scenario === 'home' ? {type: 'home'} : {type: 'session', sessionID: 'shown'};
let dialogOpen = !!config.dialogOpen;
const client = lenient({
  session: lenient(sessionApi('tui'), 'client.session'),
  event: {subscribe},
  permission: lenient({reply: permissionReply}, 'client.permission'),
  tui: composer,
}, 'client');
const api = lenient({
  options: {},
  location,
  app: lenient({version: 'fixture-2.2.5'}, 'api.app'),
  client,
  data: lenient({
    on: (type, fn) => { if (!tuiHandlers.has(type)) tuiHandlers.set(type, []); tuiHandlers.get(type).push(fn); return () => { const l = tuiHandlers.get(type); l.splice(l.indexOf(fn), 1); }; },
    listen: (fn) => { tuiListeners.add(fn); return () => tuiListeners.delete(fn); },
    session: lenient({
      list: () => [...sessions.values()],
      get: (id) => sessions.get(id),
      root: (id) => { let s = sessions.get(id); while (s?.parentID) s = sessions.get(s.parentID); return s?.id ?? id; },
      family: (id) => [...sessions.values()].filter(s => s.id === id || s.parentID === id).map(s => s.id),
      status: (id) => status.get(id) ?? 'idle',
      cost: () => 0,
      sync: async () => {}, invalidate: () => {},
      message: {list: (id) => config.messages?.[id] ?? [], get: () => undefined, sync: async () => {}, invalidate: () => {}},
      pending: {list: () => [], sync: async () => {}, invalidate: () => {}},
    }, 'api.data.session'),
  }, 'api.data'),
  ui: lenient({
    router: {current: () => route, navigate: (destination) => { log('navigate', {destination: safe(destination)}); route = destination; }, register: () => () => {}},
    dialog: lenient({get open() { return dialogOpen; }, get depth() { return dialogOpen ? 1 : 0; }}, 'api.ui.dialog'),
  }, 'api.ui'),
  keymap: lenient({mode: {current: () => (dialogOpen ? 'dialog' : 'base'), push: () => () => {}}}, 'api.keymap'),
}, 'api');

const cleanups = [];
const server = (await import(pathToFileURL(serverPath).href)).default;
if (typeof server?.setup !== 'function') throw Error('the installed server module must default-export a 2.x `setup`');
const serverCleanup = await server.setup(ctx);
if (typeof serverCleanup === 'function') cleanups.push(serverCleanup);
const tui = (await import(pathToFileURL(tuiPath).href)).default;
if (typeof tui?.setup !== 'function') throw Error('the installed 2.x TUI half must default-export `{ id, setup }`');
const tuiCleanup = await tui.setup(api);
if (typeof tuiCleanup === 'function') cleanups.push(tuiCleanup);

// The shown session as OpenCode 2.x reports it on start.
publish('session.created', {sessionID: 'shown', projectID: 'fixture-project', location, slug: 'shown'});
publish('session.status', {sessionID: 'shown', status: {type: 'idle'}});
publish('session.idle', {sessionID: 'shown'});
log('ready', {pid: process.pid});

// The rest of a turn OpenCode 2.x runs for a queued prompt, on the test's word.
const finish = (inboxID) => {
  const enqueued = fs.readFileSync(base + '.events', 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l))
    .find(e => e.kind === 'user-message' && e.id === inboxID);
  const sessionID = enqueued?.sessionID ?? 'shown';
  const assistantMessageID = 'assistant-' + inboxID;
  publish('session.step.started', {sessionID, assistantMessageID, agent: 'build', model: {providerID: 'fixture', id: 'model'}});
  publish('session.text.started', {sessionID, assistantMessageID, ordinal: 0});
  publish('session.text.ended', {sessionID, assistantMessageID, ordinal: 0, text: 'DONE'});
  publish('session.step.ended', {sessionID, assistantMessageID, finish: 'stop', cost: 0, tokens: {input: 1, output: 1, reasoning: 0, cache: {read: 0, write: 0}}});
  publish('session.execution.succeeded', {sessionID});
  publish('session.status', {sessionID, status: {type: 'idle'}});
  publish('session.idle', {sessionID});
};

let offset = 0;
let processing = false;
const timer = setInterval(async () => {
  if (processing) return;
  processing = true;
  try {
    const text = fs.existsSync(base + '.control') ? fs.readFileSync(base + '.control', 'utf8') : '';
    const lines = text.slice(offset).split('\n'); lines.pop();
    for (const line of lines) {
      offset += line.length + 1; const control = JSON.parse(line);
      if ('dialogOpen' in control) { dialogOpen = control.dialogOpen; log('dialog', {open: dialogOpen}); }
      if (control.route) route = control.route;
      if (control.event) publish(control.event, control.data);
      if (control.finish) finish(control.finish);
      if (control.dispose) { for (const fn of cleanups) await fn(); log('disposed'); }
    }
  } finally { processing = false; }
}, 20);
process.on('SIGTERM', () => { clearInterval(timer); for (const child of spawned) child.kill(); process.exit(0); });
