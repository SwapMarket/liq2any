// node --test client/test/network.test.mjs
// Choosing a network: only test networks until the escrow is on mainnet, separate storage, and both chains held to the choice.
import test from 'node:test';
import assert from 'node:assert/strict';

const LIQUID_MAIN = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';
const POLYGON = 137;
const AMOY = 80002;

// A fresh copy of the module for each stored state, as a page load gives.
let loads = 0;
async function load(stored) {
  const items = new Map(stored === undefined ? [] : [['liq2any.network', stored]]);
  globalThis.localStorage = { getItem: (k) => items.get(k) ?? null, setItem: (k, v) => items.set(k, v) };
  let reloaded = 0;
  globalThis.location = { reload: () => { reloaded += 1; } };
  const config = await import(`../site/js/config.js?load=${loads += 1}`);
  return { ...config, items, reloaded: () => reloaded };
}

test('with no escrow on mainnet, every visit gets test networks, and mainnet can’t be chosen', async () => {
  for (const stored of [undefined, 'mainnet', 'testnet']) {
    const c = await load(stored);
    assert.equal(c.mainnetAvailable(), false);
    assert.equal(c.NETWORK.name, 'testnet', String(stored));
    assert.equal(c.storageKey('liq2any.swaps'), 'liq2any.swaps');
    assert.throws(() => c.chooseNetwork('mainnet'), /not available/);
    assert.throws(() => c.chooseNetwork('regtest'), /not available/);
    assert.equal(c.reloaded(), 0);
  }
});

test('a browser with no storage still gets test networks', async () => {
  globalThis.localStorage = { getItem: () => { throw new Error('blocked'); } };
  const c = await import(`../site/js/config.js?load=${loads += 1}`);
  assert.equal(c.NETWORK.name, 'testnet');
});

test('test networks pin the Amoy deployment', async () => {
  const { readFileSync } = await import('node:fs');
  const deployed = JSON.parse(readFileSync(new URL('../../contracts/polygon/deployments/amoy.json', import.meta.url), 'utf8'));
  const { NETWORKS } = await load();
  const t = NETWORKS.testnet;
  assert.equal(t.chainId, deployed.chain_id);
  assert.equal(t.escrow, deployed.escrow);
  assert.equal(t.token, deployed.token);
  assert.equal(t.walletConnectChain, `eip155:${AMOY}`);
  assert.equal(Number(t.chainParams.chainId), AMOY, 'what a wallet is asked to add is the same chain');
  assert.equal(t.polygon.length, 2, 'two endpoints, so a claim can wait for both');
  assert.notEqual(new URL(t.polygon[0]).host, new URL(t.polygon[1]).host);
});

test('mainnet pins the real ids, has no test tools, and caps a swap at 1000 USDT0', async () => {
  const { NETWORKS } = await load();
  const { mainnet, testnet } = NETWORKS;
  assert.equal(mainnet.genesis, LIQUID_MAIN);
  assert.equal(mainnet.usdt, 'ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2');
  assert.equal(mainnet.lbtc, '6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d');
  assert.equal(mainnet.token, '0xc2132D05D31c914a87C6611C10748AEb04B58e8F', "Tether's USDT0 on Polygon");
  assert.equal(mainnet.chainId, POLYGON);
  assert.equal(mainnet.walletConnectChain, `eip155:${POLYGON}`);
  assert.equal(Number(mainnet.chainParams.chainId), POLYGON);
  assert.equal(mainnet.escrow, null, 'until the user deploys it');
  assert.equal(mainnet.faucet, null);
  assert.equal(mainnet.regtest, false);
  assert.equal(mainnet.maxSwapUnits, 1000000000n);
  assert.equal(testnet.maxSwapUnits, null);
  // Nothing of testnet's may leak through the shared defaults.
  for (const key of ['genesis', 'usdt', 'lbtc', 'token', 'chainId', 'explorer', 'walletConnectChain', 'label', 'name', 'tokenName']) {
    assert.notEqual(mainnet[key], testnet[key], key);
  }
  assert.ok(mainnet.esplora.every((url) => !/testnet/.test(url)));
  assert.ok(mainnet.polygon.every((url) => !/amoy/.test(url)) && !/amoy/.test(mainnet.polygonExplorer('tx', 'x')));
  assert.notDeepEqual(mainnet.chainParams, testnet.chainParams);
});

test('both chains must agree with the network, in both directions', async () => {
  const { NETWORKS, checkGenesis } = await load();
  const main = { ...NETWORKS.mainnet, escrow: '0x0000000000000000000000000000000000000001' };
  const t = NETWORKS.testnet;
  assert.doesNotThrow(() => checkGenesis(t, t.genesis, AMOY));
  assert.doesNotThrow(() => checkGenesis(main, LIQUID_MAIN, POLYGON));
  assert.throws(() => checkGenesis(t, LIQUID_MAIN, AMOY), /mainnet/);
  assert.throws(() => checkGenesis(t, t.genesis, POLYGON), /mainnet/);
  assert.throws(() => checkGenesis(t, t.genesis, 11155111), /chain 11155111/, 'another test chain is refused too');
  assert.doesNotThrow(() => checkGenesis({ ...t, regtest: true }, 'anything', 31337), 'a local chain, for the browser test');
  assert.throws(() => checkGenesis(main, LIQUID_MAIN, AMOY), /another network/);
  assert.throws(() => checkGenesis(main, t.genesis, POLYGON), /another network/);
  assert.throws(() => checkGenesis(NETWORKS.mainnet, LIQUID_MAIN, POLYGON), /not on mainnet yet/);
});

// WalletConnect proves the page's domain to the wallet with a hidden frame from its verify server. Blocked, every wallet says
// "Cannot verify", however the domain is registered. Only those frames are allowed.
test('the page lets WalletConnect prove its domain, and allows no other frames', async () => {
  const { readFileSync } = await import('node:fs');
  const html = readFileSync(new URL('../site/index.html', import.meta.url), 'utf8');
  const csp = html.match(/Content-Security-Policy" content="([^"]*)"/)[1];
  const frames = csp.split(';').map((d) => d.trim()).find((d) => d.startsWith('frame-src'));
  assert.equal(frames, 'frame-src https://verify.walletconnect.org https://verify.walletconnect.com');
  const bundle = readFileSync(new URL('../site/vendor/walletconnect.js', import.meta.url), 'utf8');
  for (const host of bundle.match(/https:\/\/verify\.walletconnect\.[a-z]+/g)) assert.ok(frames.includes(host), host);
});

test('Liquid confirmations: on mainnet 1 up to 100 USDT0 and 2 above it; on test networks always 2', async () => {
  const { NETWORKS, confirmationsFor } = await load();
  const main = NETWORKS.mainnet, t = NETWORKS.testnet;
  const terms = (units) => ({ polygon_amount: String(units) });
  assert.equal(confirmationsFor(terms(1000000), main), 1);
  assert.equal(confirmationsFor(terms(100000000), main), 1, 'exactly 100 is not above it');
  assert.equal(confirmationsFor(terms(100000001), main), 2);
  assert.equal(confirmationsFor(terms(1000000000), main), 2);
  for (const units of [1000000, 100000001, 1000000000]) assert.equal(confirmationsFor(terms(units), t), 2);
  assert.equal(confirmationsFor(undefined, main), 1);
});
