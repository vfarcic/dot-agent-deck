// A fake OpenCode API around the REAL installed deck plugins and watcher CLI.
// No OpenCode/model/credential. All composer calls are recorded as violations.
import fs from 'node:fs';
import cp from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { pathToFileURL } from 'node:url';
import { Worker, workerData, parentPort } from 'node:worker_threads';

const [pluginPath, base, scenario] = process.argv.slice(2);
const config = JSON.parse(fs.readFileSync(base + '.config', 'utf8'));
if(config.worker && !workerData) {
  const worker=new Worker(new URL(import.meta.url),{argv:[pluginPath,base,scenario],workerData:{fake:true}});
  worker.on('error',error=>{console.error(error);process.exit(1);});
  await new Promise(resolve=>worker.on('exit',resolve));process.exit(0);
}
// `t` (wall-clock ms) lets a failing test say how far apart two events were.
const log = (kind, data = {}) => fs.appendFileSync(base + '.events', JSON.stringify({kind, t: Date.now(), ...data}) + '\n');
const handlers = new Map();
const emit = (type, properties) => {
  for (const fn of handlers.get(type) ?? []) fn({type, properties});
};
const spawned = [];
if(process.stdin.isTTY) process.stdin.setRawMode(true);
process.stdin.on('data',chunk=>fs.appendFileSync(base+'.bytes',chunk));
const realSpawn = cp.spawn;
// With `firstWatcherDelaysPublish`, only the first watcher inherits the
// publish delay knob: a later one publishes at once.
let watchers = 0;
cp.spawn = (...args) => {
  if (config.firstWatcherDelaysPublish && args[1]?.[1] === 'wait' && watchers++ > 0) {
    const env = {...process.env};
    delete env.DOT_AGENT_DECK_INBOX_TEST_PUBLISH_DELAY_MS;
    args[2] = {...args[2], env};
  }
  const child = realSpawn(...args);
  spawned.push(child);
  log('spawn', {pid: child.pid, args: args.slice(0, 2)});
  const write = child.stdin?.write.bind(child.stdin);
  child.rawWrite = write;
  if (write) child.stdin.write = (bytes, ...rest) => {
    log('stdin', {text: bytes.toString()});
    return write(bytes, ...rest);
  };
  child.stdout?.on?.('data', (bytes) => log('stdout', {pid: child.pid, text: bytes.toString()}));
  child.on('exit', (code, signal) => log('exit', {pid: child.pid, code, signal}));
  return child;
};
syncBuiltinESMExports();
process.argv = ['node', 'opencode', ...(config.argv ?? ['--model', 'fixture/argv-model'])];
const state = {dialogOpen: !!config.dialogOpen, current: scenario === 'home' ? {name:'home'} :
  {name:'session', params:{sessionID:scenario === 'child' ? 'leaf' : 'shown'}}};
const sessions = {shown:{id:'shown'},root:{id:'root'},middle:{id:'middle',parentID:'root'},leaf:{id:'leaf',parentID:'middle'}};
let created = 0;
const disposers = [];
const api = {
  route: {get current() {return state.current;}, navigate(name, params) {
    log('navigate', {name, params}); state.current = {name, params};
  }},
  state: {session:{get(id) {return sessions[id];}, messages(id) {return config.messages?.[id] ?? [];}}},
  client: {session:{async create() {
    const id = 'created-' + (++created); sessions[id] = {id}; log('create',{id}); return {data:{id}};
  }, async get({sessionID}) {return {data:sessions[sessionID]};},
  async messages({sessionID, path}) {return {data:(config.messages?.[sessionID ?? path?.id] ?? []).map(info=>({info}))};},
  async list() {return {data:Object.values(sessions)};},
  async promptAsync(request) {
    log('prompt', {request: config.server && request.path ? {sessionID:request.path.id,...request.body} : request, raw:request});
    if (config.reject) return {error:{message:'fixture rejects'}};
    // PRD #1666 §4: the first `throwAfterPrompt` calls are taken and then fail,
    // as a lost connection does after OpenCode accepted the prompt.
    if ((config.throwAfterPrompt ?? 0) > 0) {config.throwAfterPrompt -= 1; throw new Error('socket hang up');}
    const sid = request.sessionID ?? request.path?.id;
    const body = request.body ?? request;
    const text = body.parts?.[0]?.text;
    const id = 'deck-user-' + (fs.readFileSync(base+'.events','utf8').match(/"kind":"prompt"/g) ?? []).length;
    await event('message.updated', {info:{id,sessionID:sid,role:'user',model:body.model,agent:body.agent}});
    await event('message.part.updated', {part:{id:'part-'+id,type:'text',messageID:id,sessionID:sid,text}});
    log('user-message',{id,sessionID:sid});
    return {data:undefined,response:{status:204}};
  }}},
  event: {on(type, fn) {if(!handlers.has(type))handlers.set(type, []);handlers.get(type).push(fn);return ()=>{};}},
  lifecycle: {onDispose(fn) {disposers.push(fn);}},
  tui: Object.fromEntries(['appendPrompt','submitPrompt','clearPrompt'].map(name => [name,()=>log('COMPOSER-TOUCHED',{name})])),
  ui: {dialog:{get open(){return state.dialogOpen;},get depth(){return state.dialogOpen?1:0;}}},
  mode: {current(){return state.dialogOpen?'modal':'base';}},
  app: {version:'fixture'}, kv:{get(){return undefined;}}
};
if(workerData?.preServer) {
  const pre=(await import(pathToFileURL(pluginPath+'.server.mjs'))).default;
  const server=await (pre.server ?? pre)({client:api.client,directory:process.cwd(),worktree:process.cwd(),project:{id:'fixture'}});
  await server.event({event:{type:'session.created',properties:{info:{id:'shown',directory:process.cwd()}}}});
  await server.event({event:{type:'session.status',properties:{sessionID:'shown',status:{type:'idle'}}}});
  parentPort.postMessage('announced');
  await new Promise(()=>{});
}
const imported = (await import(pathToFileURL(pluginPath))).default;
let server;
if(config.preServer) {
  const worker=new Worker(new URL(import.meta.url),{argv:[pluginPath,base,scenario],workerData:{preServer:true}});
  await new Promise((resolve,reject)=>{worker.once('message',resolve);worker.once('error',reject);});
  log('ready',{pid:process.pid,early:true});
  await new Promise(resolve=>setTimeout(resolve,2000));
}
if (config.server) {
  const ctx={client:api.client,directory:process.cwd(),worktree:process.cwd(),project:{id:'fixture'}};
  server=await (imported.server ?? imported)(ctx);
} else {
  if (typeof imported.tui !== 'function') throw Error('generated TUI plugin must export {id,tui}');
  await imported.tui(api,{},{});
  if (config.doubleLoad) {
    const copy=pluginPath+'.second.mjs';fs.copyFileSync(pluginPath,copy);
    await (await import(pathToFileURL(copy))).default.tui(api,{},{});
  }
}
const event = async (type,properties) => {
  if(type==='session.created' && properties.info) sessions[properties.info.id]=properties.info;
  emit(type,properties);if(server?.event)await server.event({event:{type,properties}});
  log('emitted',{type,properties});
};
if(config.lastUser) await event('message.updated',{info:config.lastUser});
if(config.server) await event('session.created',{info:{id:'shown',directory:process.cwd()}});
log('ready',{pid:process.pid});
let offset=0;
let processing=false;
const timer=setInterval(async ()=>{
  if(processing)return;
  processing=true;
  try {
  const text=fs.existsSync(base+'.control')?fs.readFileSync(base+'.control','utf8'):'';
  const lines=text.slice(offset).split('\n');lines.pop();
  for(const line of lines) {
    offset+=line.length+1;const control=JSON.parse(line);
    if('dialogOpen' in control){state.dialogOpen=control.dialogOpen;log('dialog',{open:state.dialogOpen});}
    if(control.route) api.route.navigate(control.route.name,control.route.params);
    if(control.event)await event(control.event,control.properties);
    if(control.dispose) {for(const fn of disposers)await fn(); log('disposed');}
    if(control.eof) {for(const child of spawned)child.stdin?.end();}
    if(control.killWatcher) {for(const child of spawned)child.kill('SIGKILL');}
    // PRD #1666 §4: a line written to the newest watcher's stdin past the
    // plugin, as a component that reports a stage before registration would.
    if(control.rawStdin) {const text=JSON.stringify(control.rawStdin)+'\n';spawned[spawned.length-1]?.rawWrite?.(text);log('raw-stdin',{text});}
    // PRD #1666 §4: a stalled event loop (a dead adapter, while it lasts).
    if(control.stall) {const until=Date.now()+control.stall;while(Date.now()<until){} log('stalled',{ms:control.stall});}
    // PRD #1666 §4: OpenCode re-running the server plugin in the same module, as
    // it does when it disposes an instance: the new instance knows nothing.
    if(control.reload && config.server) {
      server=await (imported.server ?? imported)({client:api.client,directory:process.cwd(),worktree:process.cwd(),project:{id:'fixture'}});
      await server.event({event:{type:'session.created',properties:{info:{id:'shown',directory:process.cwd()}}}});
      log('reloaded');
    }
    if(control.doorbell) {
      const id=control.userID??'fallback-user';
      const parts=[{type:'text',text:control.doorbell}];
      const message={id,sessionID:'shown',role:'user'};
      const output={message,parts};
      const hook=server?.['chat.message'];
      if(hook) await hook({sessionID:'shown',messageID:id,agent:'fixture-agent',model:{providerID:'fixture',modelID:'argv-model'}},output);
      // OpenCode saves the original array after awaiting chat.message. A reassigned
      // output.parts must not make this fixture falsely accept a broken plugin.
      log('intercepted',{available:!!hook,output:{message,parts}});
      await event('message.updated',{info:message});
      for(const part of parts)await event('message.part.updated',{part:{...part,messageID:id,sessionID:'shown'}});
      log('user-message',{id,sessionID:'shown'});
    }
  }
  } finally {processing=false;}
},20);
process.on('SIGTERM',()=>{clearInterval(timer);for(const child of spawned)child.kill();process.exit(0);});
