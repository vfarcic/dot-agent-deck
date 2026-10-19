import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

test('native observer retains delivery evidence without message or tool payloads', async () => {
 const source = readFileSync(new URL('./observe-native.ts', import.meta.url), 'utf8');
 const {default: observe} = await import(`data:text/javascript,${encodeURIComponent(source)}`);
 const root = mkdtempSync(join(tmpdir(), 'pi-observer-'));
 const previous = process.env.DECK_TEST_PI_EVENTS;
 process.env.DECK_TEST_PI_EVENTS = join(root, 'native.jsonl');
 try {
  const handlers = new Map();
  observe({on: (type, handler) => handlers.set(type, handler)});
  const id = 'm-k3xzq2ab-0f3c9a71d2e4b850';
  const repeat = `This may repeat dot-agent-deck message ${id}, which may already have reached you. If you already started it, continue that work instead of starting it again.`;
  const secret = 'PRIVATE-BODY sk-test-TOKEN cap-SECRET /home/private /var/tmp/scratch';
  const context = {sessionManager: {getSessionId: () => 'a0000000-0000-0000-0000-000000000001'}};
  for (const [type, handler] of handlers) handler({
   text: `[dot-agent-deck message ${id} subject=private]\n${repeat}\n${secret}`,
   message: {role: 'user', content: [{type: 'text', text: secret}]},
   args: {command: `dot-agent-deck inbox fetch ${secret}`},
   result: {content: secret}, token: secret, cap: secret,
  }, context);
  handlers.get('message_start')({message: {role: 'user', content: 'dot-agent-deck: message private'}}, context);
  handlers.get('session_start')({path: secret}, {sessionManager: {getSessionId: () => secret}});
  const raw = readFileSync(process.env.DECK_TEST_PI_EVENTS, 'utf8');
  for (const forbidden of ['PRIVATE-BODY', 'sk-test-TOKEN', 'cap-SECRET', '/home/private', '/var/tmp/scratch', 'subject=private', '"args"', '"result"']) assert.ok(!raw.includes(forbidden), forbidden);
  const events = raw.trim().split('\n').map(JSON.parse);
  assert.equal(events.length, 10);
  assert.deepEqual(events[0].event, {markers: [`[dot-agent-deck message ${id} `, repeat, 'inbox fetch'], message: {role: 'user'}});
  assert.deepEqual(events[8].event.markers, ['dot-agent-deck: message']);
  assert.equal(events[9].session, '[redacted]');
  assert.deepEqual(events.map(e => e.seq), Array.from({length: 10}, (_, i) => i + 1));
 } finally {
  if (previous === undefined) delete process.env.DECK_TEST_PI_EVENTS;
  else process.env.DECK_TEST_PI_EVENTS = previous;
  rmSync(root, {recursive: true});
 }
});
