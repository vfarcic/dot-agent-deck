// Passive test observer: no transforms, synthetic events, delivery or receipts.
import {appendFileSync} from 'node:fs';
export default function(pi) {
 let seq=0;
 for (const type of ['session_start','input','before_agent_start','agent_start','message_start','tool_execution_start','tool_execution_end','agent_settled'])
  pi.on(type,(event,ctx)=>appendFileSync(process.env.DECK_TEST_PI_EVENTS,
   JSON.stringify({type,pid:process.pid,session:ctx.sessionManager.getSessionId(),seq:++seq,event})+'\n'));
}
