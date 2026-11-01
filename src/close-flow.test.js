import assert from 'node:assert/strict';
import test from 'node:test';
import { createCloseRequestGuard, runCloseFlow } from './close-flow.js';

test('close flow closes exactly once only after save/discard and flush', async () => {
  const calls = [];
  const base = { hasDirty: () => true, decide: async () => 'save', saveAll: async () => { calls.push('save'); return true; }, discardAll: async () => true, flush: async () => { calls.push('flush'); return true; }, close: async () => calls.push('close') };
  assert.equal(await runCloseFlow(base), 'closed'); assert.deepEqual(calls, ['save', 'flush', 'close']);
  assert.equal(await runCloseFlow({ ...base, decide: async () => 'cancel', close: async () => { throw new Error('must not close'); } }), 'cancel');
  assert.equal(await runCloseFlow({ ...base, saveAll: async () => false }), 'save-failed');
  assert.equal(await runCloseFlow({ ...base, close: async () => { throw new Error('native close failed'); } }), 'close-failed');
  assert.equal(await runCloseFlow({ ...base, hasDirty: () => false, decide: async () => { throw new Error('clean close must not prompt'); } }), 'closed');
});

test('duplicate compositor requests share one in-flight decision, then a failed/canceled decision accepts a retry', async () => {
  let release;
  let calls = 0;
  const waiting = new Promise((resolve) => { release = resolve; });
  const guard = createCloseRequestGuard(async () => { calls += 1; await waiting; return 'cancel'; });
  const first = guard.request(1);
  const duplicate = guard.request(2);
  assert.equal(first, duplicate);
  assert.equal(guard.inFlight, true);
  release();
  assert.equal(await first, 'cancel');
  assert.equal(guard.inFlight, false);
  assert.equal(await guard.request(3), 'cancel');
  assert.equal(calls, 2);
});

test('discard suppresses subsequent recovery persistence before closing', async () => {
  const calls = []; let suppressed = false;
  const outcome = await runCloseFlow({ hasDirty: () => true, decide: async () => 'discard', saveAll: async () => true, discardAll: async () => { suppressed = true; calls.push('discard'); return true; }, flush: async () => { if (!suppressed) calls.push('persist'); calls.push('flush'); return true; }, close: async () => calls.push('close') });
  assert.equal(outcome, 'closed');
  assert.deepEqual(calls, ['discard', 'flush', 'close']);
});

test('a rejected native close after discard is retryable and never invokes close twice', async () => {
  let closes = 0;
  const failed = await runCloseFlow({ hasDirty: () => true, decide: async () => 'discard', saveAll: async () => true, discardAll: async () => true, flush: async () => true, close: async () => { closes += 1; throw new Error('rejected'); } });
  assert.equal(failed, 'close-failed'); assert.equal(closes, 1);
  const retried = await runCloseFlow({ hasDirty: () => true, decide: async () => 'discard', saveAll: async () => true, discardAll: async () => true, flush: async () => true, close: async () => { closes += 1; } });
  assert.equal(retried, 'closed'); assert.equal(closes, 2);
});
