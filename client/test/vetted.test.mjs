// node --test client/test/vetted.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { VETTED_MAKERS, anyVetted, vettedMaker } from '../site/js/vetted.js';

const entries = Object.entries(VETTED_MAKERS).flatMap(([network, list]) => list.map((m) => ({ network, ...m })));

test('every listed key can match, because a badly typed one would silently never show', () => {
  for (const m of entries) {
    assert.match(m.key, /^[0-9a-f]{64}$/, m.name);
    assert.ok(m.name.trim(), m.key);
  }
  for (const list of Object.values(VETTED_MAKERS)) assert.equal(new Set(list.map((m) => m.key)).size, list.length, 'duplicate key');
});

test('makers are listed for test networks and mainnet only, never the same key on both', () => {
  assert.deepEqual(Object.keys(VETTED_MAKERS).sort(), ['mainnet', 'testnet']);
  const test = new Set(VETTED_MAKERS.testnet.map((m) => m.key));
  for (const m of VETTED_MAKERS.mainnet) assert.ok(!test.has(m.key), `${m.name} is on both networks`);
});

test('with nobody listed, the filter has nothing to keep, so the page turns it off', () => {
  for (const network of ['testnet', 'mainnet']) assert.equal(anyVetted(network), VETTED_MAKERS[network].length > 0);
  assert.equal(anyVetted('regtest'), false);
});

test('only the whole key on the right network is vetted', () => {
  // A listed key, or a stand-in while nobody is listed, to check the matching itself.
  const key = VETTED_MAKERS.testnet[0]?.key ?? 'ab'.repeat(32);
  if (!VETTED_MAKERS.testnet.length) VETTED_MAKERS.testnet.push({ key, name: 'stand-in' });
  assert.equal(vettedMaker('testnet', key).key, key);
  assert.equal(vettedMaker('mainnet', key), null, 'a test maker is not vetted elsewhere');
  const middle = key[30] === '0' ? '1' : '0';
  assert.equal(vettedMaker('testnet', key.slice(0, 30) + middle + key.slice(31)), null, 'a key that looks the same on screen');
  assert.equal(vettedMaker('testnet', key.toUpperCase()), null);
  assert.equal(vettedMaker('testnet', key.slice(0, 32)), null);
  assert.equal(vettedMaker('testnet', ''), null);
  assert.equal(vettedMaker('testnet', undefined), null);
});
