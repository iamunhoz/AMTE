import assert from 'node:assert/strict';
import test from 'node:test';
import { campaignSeeds, requestedSeed, validateSeed } from './reliability-gate.mjs';

test('gate random seed is an unsigned 64-bit decimal and joins fixed seeds', () => {
  const { seeds, random } = campaignSeeds([], {});
  assert.equal(seeds.length, 4);
  assert.deepEqual(seeds.slice(0, 3), ['11', '29', '73']);
  assert.match(random.seed, /^(?:0|[1-9]\d{0,19})$/);
  assert.equal(validateSeed(random.seed), random.seed);
});

test('gate seed overrides replay exactly through CLI and environment', () => {
  assert.deepEqual(campaignSeeds(['--seed', '18446744073709551615'], {}).seeds, ['11', '29', '73', '18446744073709551615']);
  assert.deepEqual(campaignSeeds([], { AMTE_RELIABILITY_RANDOM_SEED: '42' }).seeds, ['11', '29', '73', '42']);
  assert.equal(requestedSeed(['--seed=9'], { AMTE_RELIABILITY_RANDOM_SEED: '42' }).source, 'cli');
  assert.throws(() => validateSeed('18446744073709551616'));
  assert.throws(() => requestedSeed(['--seed'], {}));
});
