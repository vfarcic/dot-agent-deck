// Passive observer. Persist a positive allowlist, never raw prompts, tool
// arguments/results, credentials, capabilities or paths. The marker strings
// retain the existing assertions' positive AND negative delivery evidence.
import {appendFileSync} from 'node:fs';

function markers(value) {
 const found = [];
 function visit(v) {
  if (typeof v === 'string') {
   found.push(...Array.from(v.matchAll(/\[dot-agent-deck message m-[a-z0-9]+-[a-f0-9]+ /g), m => m[0]));
   found.push(...Array.from(v.matchAll(/This may repeat dot-agent-deck message m-[a-z0-9]+-[a-f0-9]+, which may already have reached you\. If you already started it, continue that work instead of starting it again\./g), m => m[0]));
   if (v.includes('dot-agent-deck: message')) found.push('dot-agent-deck: message');
   if (v.includes('inbox fetch')) found.push('inbox fetch');
  } else if (Array.isArray(v)) v.forEach(visit);
  else if (v && typeof v === 'object') Object.values(v).forEach(visit);
 }
 visit(value);
 return [...new Set(found)];
}

export default function(pi) {
 let seq = 0;
 for (const type of ['session_start','input','before_agent_start','agent_start','message_start','tool_execution_start','tool_execution_end','agent_settled']) {
  pi.on(type, (event, ctx) => {
   const role = event.message?.role;
   const session = ctx.sessionManager.getSessionId();
   const observed = {markers: markers(event)};
   if (['user', 'assistant', 'toolResult'].includes(role)) observed.message = {role};
   appendFileSync(process.env.DECK_TEST_PI_EVENTS, JSON.stringify({
    type, pid: process.pid, session: /^[a-f0-9-]{36}$/i.test(session) ? session : '[redacted]',
    seq: ++seq, event: observed,
   }) + '\n', {mode: 0o600});
  });
 }
}
