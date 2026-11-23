// Drives the real page in a real browser, against whatever maker the caller started. Everything is real except the Polygon wallet,
// which is a test wallet that announces itself through EIP-6963 as an extension would, and lives only in this file. It sends
// through anvil, which signs for its own accounts, and for any other once told to act as it.
//
// Run through the daemon's suite:  CLIENT_TEST=1 SCENARIO="browser" cargo test -p liq2any-maker --test e2e -- --ignored --nocapture
import { createServer } from 'node:http';
import { readFileSync, existsSync, mkdtempSync, cpSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { execFile, execFileSync, execSync } from 'node:child_process';
import { promisify } from 'node:util';
import { tmpdir, homedir } from 'node:os';
import path from 'node:path';
import { chromium } from 'playwright-core';

const env = process.env;
const run = promisify(execFile);
const SITE = path.resolve(import.meta.dirname, '../site');
// A copy stamped as the deploy stamps it (client/stamp.mjs), served untouched under /stamped/.
const STAMPED = mkdtempSync(path.join(tmpdir(), 'liq2any-stamped-'));
cpSync(SITE, STAMPED, { recursive: true });
execFileSync(process.execPath, [path.resolve(import.meta.dirname, '../stamp.mjs'), STAMPED, 'test']);
const rpcArgs = JSON.parse(env.ELEMENTS_ARGS);
const cli = (...args) => run(env.ELEMENTS_CLI, [...rpcArgs, ...args], { maxBuffer: 1 << 26 }).then((r) => r.stdout.trim());
const walletCli = (...args) => cli('-rpcwallet=w', ...args);

// ---- An Esplora API over the node, with the CORS headers the real ones send ----
async function scan(address) {
  for (let i = 0; ; i++) {
    try {
      return JSON.parse(await cli('scantxoutset', 'start', JSON.stringify([`addr(${address})`])));
    } catch (e) {
      if (i > 100 || !String(e.stderr ?? e).includes('already in progress')) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

async function esplora(pathname, method, body) {
  const m = (re) => re.exec(pathname);
  let x;
  if (pathname === '/blocks/tip/height') return cli('getblockcount');
  if (pathname === '/block-height/0') return cli('getblockhash', '0');
  if ((x = m(/^\/address\/([^/]+)\/utxo$/))) {
    const found = await scan(x[1]);
    return JSON.stringify(found.unspents.map((u) => ({ txid: u.txid, vout: u.vout, status: { confirmed: true, block_height: u.height } })));
  }
  if ((x = m(/^\/tx\/([0-9a-f]+)\/hex$/))) return cli('getrawtransaction', x[1]);
  if ((x = m(/^\/tx\/([0-9a-f]+)\/status$/))) {
    const info = JSON.parse(await cli('getrawtransaction', x[1], '1'));
    return JSON.stringify({ confirmed: (info.confirmations ?? 0) > 0 });
  }
  if ((x = m(/^\/address\/([^/]+)\/txs$/))) {
    const tip = Number(await cli('getblockcount'));
    for (; scannedTo <= tip; scannedTo++) {
      const block = JSON.parse(await cli('getblock', await cli('getblockhash', String(scannedTo)), '2'));
      for (const t of block.tx) {
        for (const o of t.vout) {
          const a = o.scriptPubKey?.address;
          if (a) txsByAddress.set(a, [...(txsByAddress.get(a) ?? []), { txid: t.txid }]);
        }
      }
    }
    return JSON.stringify(txsByAddress.get(x[1]) ?? []);
  }
  if (pathname === '/tx' && method === 'POST') return cli('sendrawtransaction', body);
  throw new Error(`no such path ${pathname}`);
}

function serve(handler) {
  return new Promise((resolve) => {
    const server = createServer(handler);
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }));
  });
}

let loseBroadcastReply = false;
const txsByAddress = new Map();
let scannedTo = 0;
const shim = await serve(async (req, res) => {
  let body = '';
  for await (const chunk of req) body += chunk;
  res.setHeader('access-control-allow-origin', '*');
  try {
    const reply = await esplora(new URL(req.url, 'http://x').pathname.replace(/^\/[ab]/, ''), req.method, body);
    // The transaction went out, and the page never hears so.
    if (req.method === 'POST' && loseBroadcastReply) {
      loseBroadcastReply = false;
      req.socket.destroy();
      return;
    }
    res.end(reply);
  } catch (e) {
    res.statusCode = 400;
    res.end(String(e.stderr ?? e.message));
  }
});

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };
// ---- Polygon: anvil, and a second endpoint in front of it, as a second provider would be ----
const RPC = env.POLYGON_RPC;
async function polygonRpc(method, params = []) {
  const res = await fetch(RPC, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
  const body = await res.json();
  if (body.error) throw Object.assign(new Error(body.error.message), { code: body.error.code, data: body.error.data });
  return body.result;
}
const second = await serve(async (req, res) => {
  let body = '';
  for await (const chunk of req) body += chunk;
  res.setHeader('access-control-allow-origin', '*');
  res.setHeader('access-control-allow-headers', 'content-type');
  if (req.method === 'OPTIONS') return res.end();
  const reply = await fetch(RPC, { method: 'POST', headers: { 'content-type': 'application/json' }, body });
  res.setHeader('content-type', 'application/json');
  res.end(await reply.text());
});
const SECOND_RPC = `http://127.0.0.1:${second.port}/`;
const CHAIN_ID = Number(await polygonRpc('eth_chainId'));

const config = {
  name: env.NETWORK_NAME, regtest: true, genesis: await cli('getblockhash', '0'), usdt: env.USDT, lbtc: env.LBTC,
  esplora: [`http://127.0.0.1:${shim.port}/a`, `http://127.0.0.1:${shim.port}/b`], relays: [env.RELAY_URL],
  // One endpoint, so the claim's consent box shows. Settings adds the second, which the claim then waits on.
  polygon: [RPC],
  chainId: CHAIN_ID,
  chainParams: { chainId: `0x${CHAIN_ID.toString(16)}`, chainName: 'Anvil', nativeCurrency: { name: 'POL', symbol: 'POL', decimals: 18 }, rpcUrls: [RPC], blockExplorerUrls: ['https://example.invalid'] },
  walletConnectChain: `eip155:${CHAIN_ID}`,
  escrow: env.ESCROW,
  token: env.TOKEN,
  pollMs: 1000,
  // The suite's maker locks for 90 s on Polygon, and asks a taker's lock to leave 120 s, so that failures play out in minutes.
  claimCutoffSecs: 20,
  p2lLockMarginSecs: 300,
  rules: { l2p_min_blocks: 420, l2p_max_blocks: 540, p2l_min_blocks: 50, p2l_max_blocks: 150, l2p_min_window_secs: 30, l2p_max_window_secs: 120, p2l_max_window_secs: 1000 },
};
// The makers this page treats as vetted. The suite's maker has a new key each run, so the list is set once it's known.
let vetted = [];
const site = await serve((req, res) => {
  const pathname = new URL(req.url, 'http://x').pathname;
  if (pathname.startsWith('/stamped/')) {
    const file = path.join(STAMPED, pathname.slice('/stamped'.length).replace(/^\/$/, '/index.html'));
    if (!file.startsWith(STAMPED) || !existsSync(file)) {
      res.statusCode = 404;
      res.end();
      return;
    }
    res.setHeader('content-type', TYPES[path.extname(file)] ?? 'application/octet-stream');
    res.end(readFileSync(file));
    return;
  }
  const rel = pathname.replace(/^\/$/, '/index.html');
  const file = path.join(SITE, rel);
  if (!file.startsWith(SITE) || !existsSync(file)) {
    res.statusCode = 404;
    res.end();
    return;
  }
  let data = readFileSync(file);
  if (rel === '/js/config.js') data = `${data}\nObject.assign(NETWORK, ${JSON.stringify(config)});\n`;
  if (rel === '/js/vetted.js') data = `${data}\nVETTED_MAKERS[${JSON.stringify(env.NETWORK_NAME)}] = ${JSON.stringify(vetted.map((key) => ({ key, name: 'test' })))};\n`;
  // The real page allows https and wss only. These endpoints are local, so only this test relaxes it.
  if (rel === '/index.html') data = String(data).replace("connect-src 'self' https: wss:;", 'connect-src * ws: http:;');
  res.setHeader('content-type', TYPES[path.extname(file)] ?? 'application/octet-stream');
  res.end(data);
});

// ---- The test wallet ----
const ownerAddress = env.TAKER_POLYGON;

// A new address that has never sent anything, given test POL or not. Anvil sends for it once told to act as it.
async function freshWallet(pol = true) {
  const address = `0x${randomBytes(20).toString('hex')}`;
  await polygonRpc('anvil_impersonateAccount', [address]);
  if (pol) await polygonRpc('anvil_setBalance', [address, '0xde0b6b3a7640000']);
  return { address };
}

const executable = env.CHROME
  ?? String(execSync(`ls ${path.join(homedir(), '.cache/ms-playwright')}/chromium_headless_shell-*/*/chrome-headless-shell | head -1`)).trim();

const browser = await chromium.launch({ executablePath: executable, args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage', '--no-proxy-server'] });
const shots = mkdtempSync(path.join(env.SHOTS_DIR ?? tmpdir(), 'client-'));
console.log(`    screenshots in ${shots}`);

async function newPage(viewport = { width: 1200, height: 1000 }, beforeLoad = null, phone = false, who = { address: ownerAddress }) {
  const { deviceScaleFactor, ...size } = viewport;
  const context = await browser.newContext({ acceptDownloads: true, viewport: size, ...(deviceScaleFactor ? { deviceScaleFactor } : {}), permissions: ['clipboard-read', 'clipboard-write'], ...(phone ? { hasTouch: true, isMobile: true } : {}) });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('console', (m) => { if (m.type() === 'warning') console.log(`    page: ${m.text().slice(0, 300)}`); });
  page.errors = errors;
  // What the wallet sends goes to anvil from this address. Anything else it is asked goes to anvil as it is.
  await page.exposeFunction('testWalletRequest', async (method, params) => {
    try {
      return { result: await polygonRpc(method, params) };
    } catch (e) {
      return { error: { message: e.message, code: e.code } };
    }
  });
  if (!phone) await page.addInitScript(({ address, chainId }) => {
    const chain = `0x${chainId.toString(16)}`;
    const provider = {
      request: async ({ method, params }) => {
        if (method === 'eth_accounts') {
          window.testWalletSilent = true;
          return localStorage.getItem('test.refuseSilent') ? [] : [address];
        }
        if (method === 'eth_requestAccounts') {
          window.testWalletSilent = false;
          const refuse = localStorage.getItem('test.refuseConnect');
          if (refuse === 'person') throw Object.assign(new Error('User rejected the request'), { code: 4001 });
          if (refuse === 'broken') throw new Error('Unexpected wallet failure');
          return [address];
        }
        if (method === 'eth_chainId') return chain;
        if (method === 'wallet_switchEthereumChain' || method === 'wallet_addEthereumChain') return null;
        if (method === 'eth_sendTransaction') {
          // A person takes a moment to approve.
          await new Promise((r) => setTimeout(r, Number(localStorage.getItem('test.signDelay') ?? 0)));
          if (params[0].from.toLowerCase() !== address.toLowerCase()) throw new Error("not this wallet's account");
        }
        const { result, error } = await window.testWalletRequest(method, params);
        if (error) throw Object.assign(new Error(error.message), { code: error.code });
        return result;
      },
    };
    const announce = () => window.dispatchEvent(new CustomEvent('eip6963:announceProvider', {
      detail: Object.freeze({ info: { uuid: 'test-wallet', name: 'Test Wallet', icon: 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>', rdns: 'test.wallet' }, provider }),
    }));
    window.addEventListener('eip6963:requestProvider', announce);
    announce();
  }, { address: who.address, chainId: CHAIN_ID });
  // Only test networks exist until the escrow is on mainnet. A test can still store a choice, which the page must ignore.
  await page.addInitScript(() => { if (localStorage.getItem('liq2any.network') === null) localStorage.setItem('liq2any.network', 'testnet'); });
  if (beforeLoad) await beforeLoad(page);
  await page.goto(`http://127.0.0.1:${site.port}/`, { waitUntil: 'commit' });
  return page;
}

const step = (t) => console.log(`    ${t}`);
async function connectTestWallet(page) {
  const go = page.locator('#go', { hasText: 'Connect a Polygon wallet' });
  await go.waitFor({ timeout: 15000 });
  // With a wallet in this browser, a phone wallet is one more button away.
  await page.locator('#view .walletconnect-qr').waitFor({ timeout: 15000 });
  await go.click();
  await page.getByRole('link', { name: 'disconnect' }).waitFor({ timeout: 5000 });
  await page.locator('#view .walletconnect-qr').waitFor({ state: 'hidden', timeout: 5000 });
  // Once connected, the wallet's tUSDT0 and POL show beside it.
  await page.locator('.wallet-balances', { hasText: /^ · [\d,.]+ tUSDT0 · \d+\.\d{4} POL$/ }).first().waitFor({ timeout: 30000 });
}
const state = (page) => page.locator('.swap-card').first().getAttribute('data-state');
const waitState = (page, want, ms = 240000) => page.locator(`.swap-card[data-state="${want}"]`).first().waitFor({ timeout: ms });

// Logs get pasted into chats. After a real swap, none of its secrets may be in them, in any form the page knows.
async function checkLogs(page) {
  const { text, swaps } = await page.evaluate(() => ({
    text: localStorage.getItem('liq2any.logs') ?? '',
    swaps: Object.values(JSON.parse(localStorage.getItem('liq2any.swaps') ?? '{}')),
  }));
  if (!text.includes('->')) throw new Error('the log has no swap states in it');
  for (const s of swaps) {
    for (const v of [s.secrets.lock_secret, s.secrets.taker_sk, s.secrets.blinding_key, s.nostr.secret, s.claim_tx]) {
      if (v && text.includes(v)) throw new Error('a secret reached the log');
    }
  }
}

// Settings opens from the gear on the swap form or on a swap's card.
async function openSettings(page) {
  await page.locator('.gear:visible').first().click();
}

async function checkSettingsLogs(page) {
  await openSettings(page);
  await page.locator('#logs-download').waitFor();
  const [file] = await Promise.all([page.waitForEvent('download'), page.locator('#logs-download').click()]);
  const saved = readFileSync(await file.path(), 'utf8');
  if (!saved.includes('started, build')) throw new Error(`the downloaded log is wrong: ${saved.slice(0, 200)}`);
  await page.screenshot({ path: path.join(shots, 'settings.png') });
  await page.locator('#logs-delete').click();
  if (!(await page.locator('#logs-note').innerText()).includes('deleted')) throw new Error('the logs were not deleted');
  await page.getByRole('button', { name: 'Cancel' }).click();
}

async function checkNoErrors(page) {
  const real = page.errors.filter((e) => !/Failed to load resource|WebSocket/.test(e));
  if (real.length) throw new Error(`the page logged errors: ${real.join(' | ').slice(0, 500)}`);
}

async function upToRecovery(page, flip, to) {
  await connectTestWallet(page);
  if (flip) await page.locator('.flip').click();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, `book-${flip ? 'p2l' : 'l2p'}.png`) });
  await page.locator('#amount').fill(env.AMOUNT);
  if (to) {
    // A real Liquid testnet address, while these chains are regtest: it would pay a script only that network's wallet can spend.
    await page.locator('#to').fill('tlq1pqvd5qf9vjghhl6cd5v2t5yp6rxj9r7e8q3lxttjtt9zfvmykjk8tskcf060v44lx2gzrmpu9zad8ad400ncvuq58st3s7anpuqvpugyaaj8fe0wqwa26');
    await page.locator('#go:disabled', { hasText: 'This address is for another network' }).waitFor({ timeout: 10000 });
    await page.locator('#to').fill(to);
  }
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#accept').waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, `quote-${flip ? 'p2l' : 'l2p'}.png`) });
  await page.locator('#accept').click();
  await page.locator('#download').waitFor({ timeout: 30000 });
  step('quote accepted and terms checked');
}

async function startSwap(page, flip, to) {
  await upToRecovery(page, flip, to);
  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('the next step showed before the recovery file');
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('#download').click()]);
  const file = path.join(shots, `recovery-${Date.now()}.json`);
  await download.saveAs(file);
  await page.locator('#recovery-file').setInputFiles({ name: 'other.json', mimeType: 'application/json', buffer: Buffer.from('{"format":"nope"}') });
  await page.locator('#banner:not([hidden])').waitFor({ timeout: 5000 });
  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('a bad recovery file unlocked the next step');
  await page.locator('#recovery-file').focus();
  await page.locator('#recovery-file').setInputFiles(file);
  step('a bad file is refused, and the funding step only appears after the real one is read back');
  // The step just done shows how long it took, the current one counts up, and every address links to its explorer.
  await page.locator('.steps li.done .step-time').first().waitFor({ timeout: 30000 });
  const live = page.locator('.steps li.now .step-time.live');
  const first = await live.innerText();
  await page.waitForFunction((t) => document.querySelector('.steps li.now .step-time.live')?.textContent !== t, first, { timeout: 5000 });
  const links = await page.locator('.facts.addresses a').evaluateAll((as) => as.map((a) => a.href));
  if (!links.some((h) => /\/address\//.test(h)) || links.length < 3) throw new Error(`addresses without explorer links: ${links}`);
  step('each step shows its time, the current one counting up, and every address links to its explorer');
  return file;
}

const pay = (address, sats, label) => {
  const amount = `${Math.floor(sats / 1e8)}.${String(sats % 1e8).padStart(8, '0')}`;
  return walletCli('sendtoaddress', address, amount, '', '', 'false', 'false', '1', 'UNSET', 'false', label);
};
const amountSats = (t) => Math.round(Number(t) * 1e6) * 100;
// What the node's own wallet reads at this address, of the swap's asset, in base units.
async function receivedAt(address) {
  const coins = JSON.parse(await walletCli('listunspent', '0', '9999999', JSON.stringify([address])));
  return coins.filter((c) => c.asset === env.USDT).reduce((sum, c) => sum + Math.round(c.amount * 1e8), 0);
}

async function reportSwaps(page) {
  const points = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps'))).map((s) => s.secrets.lock_point));
  for (const point of points) console.log(`SWAP ${point}`);
}

// More than one offer can take: the maker's per-swap limit splits it into two real swaps, with one recovery file.
async function split() {
  const page = await newPage();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.locator('#amount').fill(env.SPLIT_AMOUNT);
  await connectTestWallet(page);
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#not-atomic').waitFor({ timeout: 30000 });
  await page.screenshot({ path: path.join(shots, 'split-quote.png') });
  const review = await page.locator('.card .facts').innerText();
  // 120 units goes to the first maker's dearer step, at 150 bps, and the 50 left to the dearer second maker, at 180.
  if (!review.includes('0.00012 → 0.000118') || !review.includes('0.00005 → 0.000049')) throw new Error(`wrong prices in the review: ${review}`);
  if (new Set((await page.locator('.card .facts dt.mono').allInnerTexts())).size !== 2) throw new Error(`one maker twice: ${review}`);
  step('each fill has the price for its size, from a different maker');
  step('the review says these are separate swaps, and that some may refund');
  await page.locator('#accept').click();
  const [download] = await Promise.all([page.waitForEvent('download'), page.locator('#download').click()]);
  await page.locator('#recovery-file').focus();
  await page.locator('#recovery-file').setInputFiles(await download.path());
  await page.locator('.pay-address').nth(1).waitFor({ timeout: 10000 });
  await page.screenshot({ path: path.join(shots, 'split-pay.png'), fullPage: true });
  const count = await page.locator('.pay-address').count();
  if (count !== 2) throw new Error(`${count} addresses for a split order`);
  const addresses = await page.locator('.pay-address').allInnerTexts();
  const amounts = await page.locator('.pay-amount').allInnerTexts();
  if (addresses[0] === addresses[1]) throw new Error('both swaps share an address');
  const sum = amounts.reduce((s, a) => s + Math.round(parseFloat(a) * 1e8), 0);
  if (sum !== amountSats(env.SPLIT_AMOUNT)) throw new Error(`the payments add up to ${sum}`);
  for (let i = 0; i < 2; i++) {
    await pay(addresses[i], Math.round(parseFloat(amounts[i]) * 1e8), env.USDT);
    await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  }
  step('one recovery file covers both, and two exact payments to two addresses add up to the order');
  await page.locator('.one-endpoint').first().waitFor({ timeout: 240000 });
  await page.locator('.one-endpoint').first().check();
  for (let i = 0; i < 2; i++) {
    await page.locator('.swap-card[data-state="ready_claim"] .claim').first().waitFor({ timeout: 240000 });
    await page.locator('.swap-card[data-state="ready_claim"] .claim').first().click();
    await page.locator('.swap-card[data-state="done"]').nth(i).waitFor({ timeout: 120000 });
  }
  await page.screenshot({ path: path.join(shots, 'split-done.png'), fullPage: true });
  step('both swaps claimed');
  await reportSwaps(page);
  await checkNoErrors(page);
  await page.context().close();
}

// The history shows what the swap really cost and who it was with, from the agreed terms.
async function checkHistory(page) {
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps')))[0]);
  const liquid = Number(swap.terms.liquid_amount);
  const other = Number(swap.terms.polygon_amount) * 100;
  const want = (swap.direction === 'L2P' ? liquid / other : other / liquid).toFixed(4);
  await page.locator('.tab[data-tab="history"]').click();
  const row = page.locator('.history-item').first();
  await page.locator('.history-head span').first().waitFor({ timeout: 10000 });
  const titles = (await page.locator('.history-head span').allInnerTexts()).map((t) => t.toLowerCase()).join();
  if (titles !== 'swap,status,date') throw new Error(`the history's titles: ${titles}`);
  if (await row.locator('.history-price').isVisible()) throw new Error('details should start hidden');
  await row.locator('.history-row').click();
  await row.locator('.history-price').waitFor({ timeout: 2000 });
  await new Promise((r) => setTimeout(r, 5500));
  if (!(await row.locator('.history-price').isVisible())) throw new Error('the page refreshing closed the details');
  await page.setViewportSize({ width: 360, height: 800 });
  if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error('the history scrolls sideways on a phone');
  await page.setViewportSize({ width: 1200, height: 1000 });
  await row.waitFor({ timeout: 5000 });
  const price = await row.locator('.history-price').innerText();
  if (price !== want) throw new Error(`the history says ${price}, but the terms come to ${want}`);
  if ((await row.locator('.history-maker').getAttribute('title')) !== swap.maker) throw new Error('the history has the wrong maker');
  if (!(await row.locator('.history-maker .vetted').count())) throw new Error('a vetted maker without its mark in the history');
  const coinLink = await row.locator('.explorer-coin').getAttribute('href', { timeout: 10000 });
  const blinded = /\/tx\/([0-9a-f]{64})#blinded=(\d+),([0-9a-f]{64}),([0-9a-f]{64}),([0-9a-f]{64})$/.exec(coinLink);
  if (!blinded || blinded[1] !== swap.coin.txid || blinded[2] !== String(swap.terms.liquid_amount) || blinded[3] !== env.USDT) throw new Error(`the explorer link: ${coinLink}`);
  if (!(await row.locator('.explorer-polygon', { hasText: 'Polygon claim' }).getAttribute('href')).endsWith(`/tx/${swap.claim_hash}`)) throw new Error('the Polygon link');
  if (!(await page.evaluate(() => localStorage.getItem('liq2any.logs'))).includes('blinded=') === false) throw new Error('blinders reached the log');
  await page.locator('.tab[data-tab="swap"]').click();
  step('the history fits a phone, and a row opens to show the price, the maker, and explorer links with the right amount and asset');
}

// A browser with only the file, on a swap that is over. It must never offer to pay or lock, whatever the chains are slow to say.
async function restoredPage(file, beforeLoad = null) {
  const page = await newPage(undefined, beforeLoad);
  await page.goto(`http://127.0.0.1:${site.port}/#rescue`);
  await page.locator('#rescue-file').focus();
  await page.locator('#rescue-file').setInputFiles(file);
  await page.locator('.swap-card').waitFor({ timeout: 10000 });
  return page;
}

const noSpendStep = async (page) => {
  if (await page.locator('.pay-address').count() || await page.locator('.lock').count()) throw new Error('a finished swap offered to pay or lock again');
};

async function checkFinishedRestore(file) {
  const page = await restoredPage(file);
  await page.locator('.swap-card[data-state="done"]').waitFor({ timeout: 90000 });
  await noSpendStep(page);
  await page.context().close();
}

// A restored page that can't read Polygon offers nothing, rather than a step that could pay or lock again.
async function checkUnreadable(file) {
  // Only the reads of the lock fail: the chain id, read at the start, still answers, so the page runs.
  const down = (p) => p.route((url) => url.href.startsWith(RPC), (route) => {
    const body = route.request().postDataJSON();
    if (body?.method !== 'eth_call') return route.fallback();
    return route.fulfill({ status: 500, body: 'down' });
  });
  const blind = await restoredPage(file, down);
  await blind.locator('.swap-card[data-state="restored"] .status', { hasText: "Can't read" }).waitFor({ timeout: 30000 });
  await noSpendStep(blind);
  await blind.context().close();
  step('a restored page that can\'t read Polygon offers nothing, and says why');
}

async function l2p() {
  const page = await newPage();
  const file = await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  await page.screenshot({ path: path.join(shots, 'pay.png') });
  const address = await page.locator('.pay-address').innerText();
  if ((await page.locator('.pay-address').evaluate((e) => e.textContent)) !== address.replace(/\s/g, '')) throw new Error('the address shows altered');
  // A redraw can swap the element out while it is measured, which leaves nothing to measure, so look again.
  let lastLine = null;
  for (let i = 0; i < 10 && lastLine === null; i++) {
    lastLine = await page.locator('.pay-address').evaluate((e) => {
      const r = document.createRange();
      r.selectNodeContents(e);
      const rects = [...r.getClientRects()];
      return rects.length ? rects.at(-1).width / rects[0].width : null;
    });
  }
  if (lastLine < 0.1) throw new Error('a lone character on the address\'s last line');
  for (const [cls, name] of [['.pay-amount', 'Amount'], ['.pay-address', 'Address']]) {
    await page.locator(cls).click();
    await page.locator('#toast', { hasText: `${name} copied` }).waitFor({ timeout: 2000 });
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    const shown = await page.locator(cls).innerText();
    if (cls === '.pay-address' ? copied !== shown : !(shown.startsWith(`${copied} `) && /^\d+\.\d{8}$/.test(copied))) throw new Error(`${shown} copied ${copied}`);
  }
  step('clicking the amount or the address copies it, with a notification');
  // Tapped, a QR code fills the screen as its own layer, so the 15 s redraw underneath leaves it be; a tap closes it.
  await page.locator('.qr[data-enlarge]').first().click();
  await page.locator('.qr-full > svg').waitFor({ timeout: 5000 });
  await page.evaluate(() => window.dispatchEvent(new HashChangeEvent('hashchange')));
  await new Promise((r) => setTimeout(r, 1000));
  if (!(await page.locator('.qr-full').isVisible())) throw new Error('a redraw closed the enlarged code');
  await page.locator('.qr-full').click();
  await page.locator('.qr-full').waitFor({ state: 'detached', timeout: 5000 });
  step('the payment QR code fills the screen when tapped, and a tap closes it');
  await pay(address, amountSats(env.AMOUNT), env.USDT);
  step(`paid ${env.AMOUNT} from the node's wallet to the address the page derived`);
  await page.locator('.confirmations', { hasText: /Your payment has [0-2]\/2 confirmations/ }).waitFor({ timeout: 60000 });
  step('while the maker waits, the page counts the payment\'s confirmations');
  await waitState(page, 'ready_claim');
  await page.screenshot({ path: path.join(shots, 'claim.png') });
  step('the page signed the maker’s claim, and sees its lock');
  const setSecond = async (url) => {
    await openSettings(page);
    await page.locator('#s-p2').fill(url);
    await page.locator('dialog .btn', { hasText: 'Save' }).click();
  };
  await setSecond(SECOND_RPC);
  await page.locator('.one-endpoint').waitFor({ state: 'detached', timeout: 5000 });
  await setSecond('');
  await page.locator('.one-endpoint').waitFor({ timeout: 5000 });
  step('Settings opens from the gear on the claim page, and a second endpoint set there takes effect at once');
  if (!(await page.locator('.claim').isDisabled())) throw new Error('Claim is on with one endpoint and no consent');
  if (await state(page) !== 'ready_claim') throw new Error('claimed with one endpoint and no consent');
  step('with one Polygon endpoint and the box unticked, Claim stays off and the secret stays hidden');
  await page.locator('.one-endpoint').check();
  // A slow endpoint: the press shows at once, not only when the wallet is asked.
  const slow = (url) => url.href.startsWith(RPC);
  await page.route(slow, async (route) => { await new Promise((r) => setTimeout(r, 3000)); await route.fallback(); });
  await page.locator('.claim:not([disabled])').click({ timeout: 5000 });
  await page.locator('.pressed', { hasText: 'Checking the lock' }).waitFor({ timeout: 1000 });
  if (await page.locator('.claim').count()) throw new Error('Claim can be pressed twice');
  await page.unroute(slow);
  step('a pressed Claim says so at once, even while the endpoint is slow');
  // Once sent, the card says so until Polygon finalizes it, rather than offering the button again as if the press hadn't counted.
  await page.locator('.claim-sent, .swap-card[data-state="done"]').first().waitFor({ timeout: 60000 });
  if (await page.locator('.swap-card[data-state="ready_claim"] .claim').count()) throw new Error('Claim was offered again once the claim was sent');
  await waitState(page, 'done');
  step('a sent claim says it is waiting for Polygon, and the swap finishes without another press');
  await page.screenshot({ path: path.join(shots, 'done.png') });
  const claimed = await storedSwap(page);
  const gas = BigInt(await polygonRpc('eth_getBalance', [ownerAddress, 'latest']));
  if (!claimed.claim_hash || gas === 0n) throw new Error('no claim was sent, or the wallet has no POL left');
  step('claimed on Polygon');
  const l2pSteps = await page.locator('.swap-card[data-state="done"] .steps li').allInnerTexts();
  const paid = l2pSteps.findIndex((t) => t.startsWith('Pay the swap address'));
  if (!l2pSteps[paid + 1]?.startsWith('Your payment confirms') || !l2pSteps[paid + 2]?.startsWith('The maker locks on Polygon')) throw new Error(`the steps were ${l2pSteps.join(' | ')}`);
  step('waiting for the payment\'s confirmations is a step of its own, between paying and the maker\'s lock');
  // A finished swap's button says OK, and it brings the form back, as the logo does from a swap's page.
  await page.locator('#order-back', { hasText: 'OK' }).click();
  await page.locator('#amount').waitFor({ timeout: 5000 });
  await page.locator('.tab[data-tab="history"]').click();
  await page.locator('.history-item .history-row').first().click();
  await page.locator('.history-item').first().getByRole('button', { name: 'Open' }).click();
  await page.locator('.swap-card[data-state="done"]').waitFor({ timeout: 5000 });
  await page.locator('a.logo').click();
  await page.locator('#amount').waitFor({ timeout: 5000 });
  if (await page.locator('.swap-card').count()) throw new Error('the logo left the swap on the page');
  // Folded again, as the History checks below expect to find it.
  await page.locator('.tab[data-tab="history"]').click();
  await page.locator('.history-item .history-row').first().click();
  await page.locator('.tab[data-tab="swap"]').click();
  step('OK on a finished swap, and the logo from any swap, bring back a working swap form');
  await checkLogs(page);
  step('the log has the swap’s steps, and none of its secrets');
  await reportSwaps(page);
  await checkHistory(page);
  await checkFinishedRestore(file);
  step('restoring a finished swap from its file shows it finished, and never offers to pay again');
  await checkNoErrors(page);
  await page.context().close();
}

async function p2l() {
  const page = await newPage();
  const to = await walletCli('getnewaddress');
  const file = await startSwap(page, true, to);
  await page.locator('.lock').waitFor();
  await page.screenshot({ path: path.join(shots, 'lock.png') });
  const before = await tokenBalance(ownerAddress);
  await page.locator('.lock').click();
  await page.locator('.confirmations', { hasText: /Locked\. Waiting for the maker|The maker locked on Liquid: [0-2]\/2 confirmations/ }).waitFor({ timeout: 60000 });
  step('approved the escrow, then locked on Polygon');
  loseBroadcastReply = true;
  await waitState(page, 'done');
  if (loseBroadcastReply) throw new Error('the claim was never published through the page');
  await page.screenshot({ path: path.join(shots, 'p2l-done.png') });
  step('the page signed with the maker, and its claim confirmed on Liquid, even with the reply to its broadcast lost');
  const p2lSteps = await page.locator('.swap-card[data-state="done"] .steps li').allInnerTexts();
  const makerLocked = p2lSteps.findIndex((t) => t.startsWith('The maker locks on Liquid'));
  if (!p2lSteps[makerLocked + 1]?.startsWith('The maker\'s lock confirms') || !p2lSteps[makerLocked + 2]?.startsWith('Sign the claim together')) throw new Error(`the steps were ${p2lSteps.join(' | ')}`);
  step('waiting for the maker\'s lock to confirm is a step of its own, before signing the claim');
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps')))[0]);
  if (!(swap.maker_confirmations >= 2)) throw new Error(`the maker's lock was counted at ${swap.maker_confirmations} confirmations`);
  const tx = JSON.parse(await cli('getrawtransaction', swap.claim_txid, '1'));
  const coinInput = tx.vin[0].txinwitness;
  if (coinInput.length !== 1) throw new Error(`a claim's witness has ${coinInput.length} items`);
  if (tx.vout.some((o) => o.scriptPubKey?.type !== 'fee' && o.value !== undefined)) throw new Error('an output is visible');
  step('on-chain it is one signature, and everything but the fee is hidden');
  await checkLogs(page);
  const got = await receivedAt(to);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`received ${got} units, expected ${swap.terms.liquid_amount}`);
  step('the wallet unblinded exactly the agreed amount at the address it gave');
  const spent = before - (await tokenBalance(ownerAddress));
  if (spent !== BigInt(swap.terms.polygon_amount)) throw new Error(`the lock took ${spent} units, expected ${swap.terms.polygon_amount}`);
  step('the lock took exactly the agreed tUSDT0 from the wallet');
  await checkFinishedRestore(file);
  step('restoring a finished swap from its file shows it finished, and never offers to lock again');
  await checkUnreadable(file);
  await reportSwaps(page);
  await checkNoErrors(page);
  await page.context().close();
}

// A taker who pays, sees the maker lock, and never claims. The maker refunds, and this browser loses everything but the file.
async function rescue() {
  const page = await newPage();
  const file = await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  await pay(await page.locator('.pay-address').innerText(), amountSats(env.AMOUNT), env.USDT);
  await waitState(page, 'ready_claim');
  step('the maker locked, and this taker never claims');
  await page.context().close();

  const fresh = await newPage();
  await fresh.goto(`http://127.0.0.1:${site.port}/#rescue`);
  await fresh.locator('#rescue-file').focus();
  await fresh.locator('#rescue-file').setInputFiles(file);
  await fresh.locator('.swap-card').waitFor({ timeout: 10000 });
  step('a browser with no stored data picked the swap up from the recovery file');
  await fresh.locator('.swap-card[data-state="refund_wait"], .swap-card[data-state="refund"]').waitFor({ timeout: 240000 });
  step('the maker refunded on Polygon, so the page moved to a refund');
  const swap = await fresh.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps')))[0]);
  await walletCli('generatetoaddress', String(Math.max(0, swap.terms.lock_height - Number(await cli('getblockcount')) + 1)), await walletCli('getnewaddress'));
  await waitState(fresh, 'refund');
  await fresh.screenshot({ path: path.join(shots, 'refund.png') });
  await pay(await fresh.locator('.fee-address').innerText(), 30, env.LBTC);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  await new Promise((r) => setTimeout(r, 3000));
  const back = await walletCli('getnewaddress');
  await fresh.locator('.refund-to').fill(back);
  loseBroadcastReply = true;
  await fresh.locator('.refund').click();
  await fresh.locator('.swap-card[data-state="refunding"], .swap-card[data-state="refunded"]').waitFor({ timeout: 30000 });
  if (loseBroadcastReply) throw new Error('the refund was never published through the page');
  step('a refund still goes through with the reply to its broadcast lost');
  await waitState(fresh, 'refunded');
  const got = await receivedAt(back);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`the refund paid ${got} units`);
  step('the refund went through the Simplicity leaf and paid the exact amount back');
  await checkNoErrors(fresh);
  await fresh.context().close();
}

// Runs first: it finds the suite's maker, and every other scenario then trades with that maker as a vetted one.
async function vetting() {
  // Nobody is listed yet: no switch, since it would hide everyone, and a warning beside every maker.
  const page = await newPage();
  const row = page.locator('tbody tr').first();
  await row.waitFor({ timeout: 30000 });
  if (await page.locator('#only-vetted').count()) throw new Error('a vetted-only switch with nobody vetted');
  await page.locator('#unvetted-warning').waitFor({ timeout: 3000 });
  if (await page.locator('.vetted').count()) throw new Error('a checkmark on a maker that isn\'t listed');
  const key = await row.locator('td.mono').getAttribute('title');
  if (!/^[0-9a-f]{64}$/.test(key)) throw new Error(`the row's key: ${key}`);
  await checkNoErrors(page);
  await page.context().close();
  step('with nobody vetted, every maker shows, with a warning and no switch');

  // Same start and end, so the same on screen. Listed, it turns the switch on, which hides the real maker.
  vetted = [`${key.slice(0, 30)}${key[30] === '0' ? '1' : '0'}${key.slice(31)}`];
  const lookalike = await newPage();
  await lookalike.locator('.empty', { hasText: 'hidden by the switches' }).waitFor({ timeout: 30000 });
  if (!(await lookalike.locator('#only-vetted').isChecked())) throw new Error('"only vetted" should start on once someone is listed');
  if (await lookalike.locator('#unvetted-warning').isVisible()) throw new Error('a warning with the switch on');
  await connectTestWallet(lookalike);
  await lookalike.locator('#amount').fill(env.AMOUNT);
  if (await lookalike.locator('#go:not(:disabled)').count()) throw new Error('an order could go to a hidden maker');
  if (await lookalike.locator('#send-max').isVisible()) throw new Error('send limits from makers that are hidden');
  step('a listed key that only looks like the maker\'s doesn\'t count, and a hidden maker can\'t be traded with');
  await lookalike.locator('#only-vetted').uncheck();
  await lookalike.locator('tbody tr').first().waitFor({ timeout: 3000 });
  await lookalike.locator('#unvetted-warning').waitFor({ timeout: 3000 });
  await lookalike.reload();
  await lookalike.locator('tbody tr').first().waitFor({ timeout: 30000 });
  if (await lookalike.locator('#only-vetted').isChecked()) throw new Error('turning it off did not survive a reload');
  await lookalike.locator('#only-vetted').check();
  await lookalike.context().close();
  step('with the switch off, the maker shows with a warning, and that survives a reload');

  // Now both makers, which the later scenarios split orders across.
  const everyone = await newPage();
  await everyone.locator('#only-vetted').uncheck({ timeout: 30000 });
  await everyone.locator('tbody tr').nth(1).waitFor({ timeout: 30000 });
  const keys = await everyone.locator('tbody td.mono').evaluateAll((all) => all.map((td) => td.title));
  if (!keys.includes(key) || keys.length !== 2) throw new Error(`the makers: ${keys}`);
  await everyone.context().close();
  vetted = keys;
  const listed = await newPage();
  await listed.locator('tbody tr .vetted').nth(1).waitFor({ timeout: 30000 });
  await listed.context().close();
  step('once listed, the maker has its checkmark with the switch on');
}

// The library is held back here, so this shows the dialog doesn't wait for it, and needs no relay.
async function walletConnectWait() {
  const page = await newPage();
  let release;
  const held = new Promise((r) => { release = r; });
  await page.route('**/vendor/walletconnect.js', async (route) => { await held; await route.abort(); });
  await page.evaluate(() => localStorage.setItem('test.refuseConnect', 'broken'));
  await page.locator('#go', { hasText: 'Connect a Polygon wallet' }).click();
  await page.locator('dialog .spinner').waitFor({ timeout: 2000 });
  const turning = await page.locator('dialog .spinner').evaluate((e) => getComputedStyle(e).animationName);
  if (turning !== 'spin') throw new Error(`the spinner isn't animated: ${turning}`);
  // Closing it ends the attempt, without a message, and Connect works again, with no reload.
  await page.locator('dialog .btn', { hasText: 'Close' }).click();
  await page.locator('dialog').waitFor({ state: 'detached', timeout: 5000 });
  if (await page.locator('#banner:not([hidden])').count()) throw new Error('closing the dialog showed an error');
  await page.locator('#go', { hasText: 'Connect a Polygon wallet' }).click();
  await page.locator('dialog .spinner').waitFor({ timeout: 2000 });
  step('closing the WalletConnect dialog stops it quietly, and Connect works again at once');
  release();
  await page.locator('dialog').waitFor({ state: 'detached', timeout: 10000 });
  await page.locator('#banner').waitFor({ timeout: 5000 });
  await page.context().close();
  step('a wallet that fails falls back to WalletConnect, whose spinner shows at once, and a failure closes it with a message');
}

// Nothing is rebuilt: marks put on the elements must outlast typing, a flip, new offers and the page's own timers.
async function liveBook() {
  const page = await newPage();
  const row = page.locator('tbody tr').first();
  await row.waitFor({ timeout: 30000 });
  const headers = async () => (await page.locator('thead th').allInnerTexts()).map((t) => t.toLowerCase());
  const cells = async () => row.locator('td').allInnerTexts();
  if ((await headers()).join() !== 'maker,price,size') throw new Error(`columns: ${await headers()}`);
  const quoted = await cells();
  const steps = quoted[1].split('\n');
  if (!/^≥[\d,.]+\n≤[\d,.]+$/.test(quoted[2]) || steps.length < 2 || !steps.every((t, i) => (i ? /^>[\d,.]+: \d\.\d{4}$/ : /^≤[\d,.]+: \d\.\d{4}$/).test(t))) throw new Error(`before an order: ${quoted}`);
  const mark = () => page.evaluate(() => {
    for (const s of ['.card', '.book', 'tbody tr', 'tbody tr td:nth-child(2)', '#amount', '#go']) document.querySelector(s).dataset.mark = '1';
  });
  const marked = () => page.evaluate(() => ['.card', '.book', 'tbody tr', 'tbody tr td:nth-child(2)', '#amount', '#go'].filter((s) => !document.querySelector(s).dataset.mark));
  await mark();

  await page.locator('#amount').pressSequentially(env.AMOUNT);
  if ((await page.locator('#amount').inputValue()) !== env.AMOUNT) throw new Error('typing was lost');
  if (!(await page.locator('#amount').evaluate((e) => e === document.activeElement))) throw new Error('typing lost focus');
  if ((await headers()).join() !== 'maker,price,your fill') throw new Error(`columns with an order: ${await headers()}`);
  const ordered = await cells();
  const average = await page.locator('.facts dd').nth(1).innerText();
  if (ordered[1] !== average) throw new Error(`the row's price ${ordered[1]} isn't the order's ${average}`);
  if (!/^\d+\.\d{2}$/.test(ordered[2]) || ordered[2] === '0.00') throw new Error(`the fill: ${ordered[2]}`);

  await new Promise((r) => setTimeout(r, 6000));
  await page.locator('.flip').click();
  await page.locator('.flip').click();
  let lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);

  await page.locator('#amount').fill('');
  if ((await headers())[2] !== 'size') throw new Error('the caption did not come back');
  if ((await cells())[1] !== quoted[1]) throw new Error('the quoted price did not come back');
  lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);

  for (const [link, name] of [['#send-min', 'Send min'], ['#send-max', 'Send max']]) {
    const shown = await page.locator(link).innerText();
    await page.locator(link).click();
    const amount = await page.locator('#amount').inputValue();
    if (!shown || Number(shown.replaceAll(',', '')) !== Number(amount)) throw new Error(`${name} shows ${shown} but put ${amount} in the form`);
    if (await page.locator('#unfilled').isVisible()) throw new Error(`${name} left part of the order unfilled`);
    if (!(await page.locator('.facts').isVisible())) throw new Error(`no maker takes ${name}`);
    if ((await headers())[2] !== 'your fill') throw new Error(`${name} did not count as an order`);
  }
  if (!(await page.locator('#clear-amount').isVisible())) throw new Error('nothing to clear the amount with');
  await page.locator('#clear-amount').click();
  if ((await page.locator('#amount').inputValue()) !== '') throw new Error('the amount was not cleared');
  if ((await headers())[2] !== 'size') throw new Error('clearing the amount did not count as no order');
  if (await page.locator('#clear-amount').isVisible()) throw new Error('a clear button with nothing to clear');
  lost = await marked();
  if (lost.length) throw new Error(`rebuilt: ${lost}`);
  await checkNoErrors(page);
  await page.context().close();
  step('send min and max put an order in the form that the book fills completely');
  step('the book updates in place, and shows an order\'s own price and size under "your fill"');
}

// The WebAssembly is the slow part of loading, so it is held back here.
async function pageLoading() {
  let release;
  const held = new Promise((r) => { release = r; });
  const page = await newPage(undefined, (p) => p.route('**/*.wasm', async (route) => { await held; await route.fallback(); }));
  await page.locator('.page-loading .spinner').waitFor({ timeout: 5000 });
  if ((await page.locator('.page-loading .spinner').evaluate((e) => getComputedStyle(e).animationName)) !== 'spin') throw new Error('the spinner isn\'t animated');
  if (await page.locator('#amount').count()) throw new Error('the form appeared before it could work');
  const link = page.locator('#build');
  if ((await link.getAttribute('target')) !== '_blank') throw new Error('the build link stays in this tab');
  // A deploy stamps the commit. This copy isn't stamped, so it should say so, and link to the repo.
  const label = await link.innerText();
  const href = await link.getAttribute('href');
  if (!(label === 'local' ? href === 'https://github.com/SwapMarket/liq2any' : href.endsWith(`/commit/${label}`))) throw new Error(`the build link: ${label} ${href}`);
  release();
  await page.locator('#amount').waitFor({ timeout: 15000 });
  if (await page.locator('.page-loading').count()) throw new Error('the placeholder stayed');
  await page.context().close();

  const broken = await newPage(undefined, (p) => p.route('**/*.wasm', (route) => route.abort()));
  await broken.locator('#banner').waitFor({ timeout: 15000 });
  await broken.locator('.page-loading').waitFor({ state: 'detached', timeout: 5000 });
  await broken.context().close();
  step('a spinner shows while the page loads, and goes away if loading fails');
}

// Reloading mid-swap shouldn't mean connecting again, or a prompt, and disconnecting has to stick.
async function walletStays() {
  const page = await newPage();
  const connected = page.getByRole('link', { name: 'disconnect' });
  const button = page.locator('#go', { hasText: 'Connect a Polygon wallet' });
  await button.waitFor({ timeout: 15000 });
  if (await button.isDisabled()) throw new Error('the connect button should be clickable before any amount');
  await button.click();
  await connected.waitFor({ timeout: 5000 });
  await page.reload();
  await connected.waitFor({ timeout: 15000 });
  if (!(await page.evaluate(() => window.testWalletSilent))) throw new Error('the reconnect could have shown the wallet\'s prompt');

  await page.evaluate(() => localStorage.setItem('test.refuseSilent', '1'));
  await page.reload();
  await button.waitFor({ timeout: 15000 });
  await new Promise((r) => setTimeout(r, 1000));
  if (await connected.isVisible()) throw new Error('connected though the wallet refused');
  if (await page.locator('#banner').isVisible()) throw new Error('a refused reconnect should be quiet');
  await page.evaluate(() => localStorage.removeItem('test.refuseSilent'));

  await button.click();
  await connected.click();
  await page.reload();
  await button.waitFor({ timeout: 15000 });
  await new Promise((r) => setTimeout(r, 4000));
  if (await connected.isVisible()) throw new Error('disconnecting did not stick');
  await page.context().close();
  step('the wallet reconnects silently after a reload, unless refused or disconnected');
}

// The wallet comes up short, as the Polygon endpoint reports it, so no quote should even be asked for.
async function p2lBalance() {
  const zero = `0x${'0'.repeat(64)}`;
  for (const [short, want] of [
    [(b) => b?.method === 'eth_call' && b.params[0].data.startsWith(BALANCE_OF), /holds 0 tUSDT0, and this order needs/],
    [(b) => b?.method === 'eth_getBalance', /needs a little POL to pay for each lock/],
  ]) {
    const page = await newPage();
    await page.route((url) => url.href.startsWith(RPC), async (route) => {
      const body = route.request().postDataJSON();
      if (!short(body)) return route.fallback();
      return route.fulfill({ json: { jsonrpc: '2.0', id: body.id, result: body.method === 'eth_call' ? zero : '0x0' } });
    });
    await connectTestWallet(page);
    await page.locator('.flip').click();
    await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
    await page.locator('#amount').fill(env.AMOUNT);
    await page.locator('#to').fill(await walletCli('getnewaddress'));
    await page.locator('#go:not(:disabled)').click();
    await page.locator('#banner:not([hidden])', { hasText: want }).waitFor({ timeout: 10000 });
    await page.locator('#go').waitFor({ timeout: 3000 });
    if (await page.locator('#accept').count()) throw new Error('quoted an order the wallet can\'t pay');
    await page.context().close();
  }
  step('P to L stops before a quote when the wallet is short of tUSDT0 or POL');
}

async function phoneWallets() {
  const page = await newPage({ width: 400, height: 800 }, (p) => p.route('**/vendor/walletconnect.js', (route) => route.abort()), true);
  await page.locator('.connect-wallet, #go', { hasText: 'Connect a Polygon wallet' }).first().click();
  await page.locator('dialog', { hasText: 'WalletConnect' }).waitFor({ timeout: 15000 });
  await page.screenshot({ path: path.join(shots, 'phone-wallets.png') });
  await page.locator('dialog .btn', { hasText: 'Close' }).click().catch(() => {});
  await page.locator('dialog').waitFor({ state: 'detached', timeout: 15000 });

  const history = page.locator('.tab[data-tab="history"]');
  if (await history.isVisible()) throw new Error('the menu should start collapsed on a phone');
  await page.locator('#burger').click();
  await history.waitFor({ timeout: 2000 });
  if ((await page.locator('#burger').getAttribute('aria-expanded')) !== 'true') throw new Error('the burger should say it is open');
  await page.screenshot({ path: path.join(shots, 'phone-menu.png') });
  await history.click();
  await history.waitFor({ state: 'hidden', timeout: 2000 });
  if (!page.url().endsWith('#history')) throw new Error(`the menu went to ${page.url()}`);
  if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error('the phone page scrolls sideways');
  await page.context().close();

  const wide = await newPage();
  await wide.locator('.tab[data-tab="history"]').waitFor({ timeout: 15000 });
  if (await wide.locator('#burger').isVisible()) throw new Error('a burger on a wide screen');
  await wide.context().close();
  step('on a phone the menu collapses to a burger, and a link closes it');
  step('a phone with no wallet in its browser goes to WalletConnect');
}

// The wallets are tried in the order they announce themselves, and a person saying no to one stops there.
async function connectOrder() {
  const page = await newPage(undefined, (p) => p.addInitScript(() => {
    const provider = { request: async () => { throw new Error('the second wallet should never be asked'); } };
    const announce = () => window.dispatchEvent(new CustomEvent('eip6963:announceProvider', {
      detail: Object.freeze({ info: { uuid: 'other', name: 'Other Wallet', icon: 'data:image/svg+xml,<svg xmlns="http://www.w3.org/2000/svg"/>', rdns: 'other.wallet' }, provider }),
    }));
    window.addEventListener('eip6963:requestProvider', announce);
    announce();
  }));
  const go = page.locator('#go', { hasText: 'Connect a Polygon wallet' });
  await page.evaluate(() => localStorage.setItem('test.refuseConnect', 'person'));
  await go.click();
  await page.locator('#banner:not([hidden])', { hasText: 'Test Wallet declined' }).waitFor({ timeout: 5000 });
  if (await page.getByRole('link', { name: 'disconnect' }).count()) throw new Error('moved on to another wallet after a person said no');
  await page.evaluate(() => localStorage.removeItem('test.refuseConnect'));
  await go.click();
  await page.locator('.note', { hasText: 'Polygon: Test Wallet' }).waitFor({ timeout: 5000 });
  await checkNoErrors(page);
  await page.context().close();
  step('the first wallet is tried, and saying no to it stops there');
}

// Nothing goes wrong by cancelling, and nothing that has really started can be cancelled. The refusals answer from fake chain
// data, so no funds are needed, and the rest is real.
async function cancelling() {
  const confirm = async (page) => {
    await page.locator('#cancel-order').click();
    await page.locator('#cancel-confirm').click();
  };
  const refused = async (page, text, state_) => {
    await page.locator('#banner:not([hidden])', { hasText: text }).waitFor({ timeout: 10000 });
    await page.locator(`.swap-card[data-state="${state_}"]`).waitFor({ timeout: 5000 });
  };

  const page = await newPage();
  await upToRecovery(page, false);
  if (!(await page.locator('#cancel-order').isVisible())) throw new Error('no cancel while saving the recovery file');
  if (await page.getByRole('button', { name: 'Back', exact: true }).count()) throw new Error('back beside cancel');

  // A wallet's browser: no file to save, and maybe no clipboard either.
  await page.evaluate(() => { navigator.clipboard.writeText = () => Promise.reject(new Error('blocked')); });
  await page.locator('#copy-recovery').click();
  const text = await page.locator('#recovery-text').inputValue();
  if (!(await page.locator('#recovery-text').isVisible()) || JSON.parse(text).format !== 'liq2any-recovery') throw new Error('the text to copy by hand');
  // Flip the first digit to one it isn't, or one time in sixteen the text is unchanged and the step passes.
  await page.locator('#recovery-paste').fill(text.replace(/"lock_secret": "(.)/, (_, c) => `"lock_secret": "${c === '0' ? '1' : '0'}`));
  await page.locator('#recovery-paste-go').click();
  await page.locator('#banner:not([hidden])', { hasText: 'damaged' }).waitFor({ timeout: 5000 });
  if (await page.locator('.pay-address').count()) throw new Error('altered text unlocked the next step');
  await page.locator('#recovery-paste').fill(text);
  await page.locator('#recovery-paste-go').click();
  await page.locator('.pay-address').waitFor({ timeout: 10000 });
  step('with no download or clipboard, the text can be copied by hand, and altered text is refused');

  // Nothing changes while the swap waits for a payment, so a check redraws nothing: the card on screen stays the same element.
  await page.evaluate(() => { window.cardBefore = document.querySelector('.swap-card'); });
  await new Promise((r) => setTimeout(r, 16000));
  if (!(await page.evaluate(() => window.cardBefore === document.querySelector('.swap-card') && window.cardBefore.isConnected))) throw new Error('a check with nothing new drew the card again');
  step('a check with nothing new leaves the swap card as it is, so the page doesn\'t blink');

  await page.locator('#cancel-order').click();
  await page.locator('#cancel-keep').click();
  await page.locator('.swap-card[data-state="fund"]').waitFor({ timeout: 2000 });

  await page.route('**/address/*/txs', (route) => route.fulfill({ json: [{ txid: 'seen' }], headers: { 'access-control-allow-origin': '*' } }));
  await confirm(page);
  await refused(page, 'already reached', 'fund');
  await page.unroute('**/address/*/txs');
  await page.route('**/address/*/txs', (route) => route.abort());
  await confirm(page);
  await page.locator('.swap-card[data-state="fund"]').waitFor({ timeout: 5000 });
  await new Promise((r) => setTimeout(r, 1000));
  if ((await state(page)) !== 'fund') throw new Error('cancelled without being able to check the chain');
  await page.unroute('**/address/*/txs');
  step('a swap the chain shows as paid can\'t be cancelled, nor one the page can\'t check');

  // A request already in flight can still meet the failure the test just lifted, so try again as a person would.
  for (let i = 0; i < 8 && await page.locator('.swap-card[data-state="fund"]').count(); i++) {
    await new Promise((r) => setTimeout(r, 2500));
    await confirm(page);
    await page.locator('.swap-card[data-state="fund"]').waitFor({ state: 'detached', timeout: 10000 }).catch(() => {});
  }
  await page.locator('.swap-card[data-state="fund"]').waitFor({ state: 'detached', timeout: 1000 });
  await page.locator('.tab[data-tab="history"]').click();
  const row = page.locator('.history-item', { hasText: 'cancelled' });
  await row.waitFor({ timeout: 5000 });

  // A payment could still arrive, so the recovery text picks a cancelled swap back up, by asking the chains.
  await page.locator('.tab[data-tab="rescue"]').click();
  await page.locator('#rescue-paste').fill(text);
  await page.locator('#rescue-paste-go').click();
  await page.locator('.swap-card[data-state="fund"]').waitFor({ timeout: 30000 });
  step('a cancelled swap can be rescued with pasted text');

  await confirm(page);
  await page.locator('.swap-card[data-state="fund"]').waitFor({ state: 'detached', timeout: 10000 });
  await page.locator('.tab[data-tab="history"]').click();
  await row.locator('.history-row').click();
  await row.getByRole('button', { name: 'Forget' }).click();
  await row.waitFor({ state: 'detached', timeout: 5000 });
  await checkNoErrors(page);
  await page.context().close();
  step('once cancelled, a swap can be forgotten');

  const early = await newPage();
  await upToRecovery(early, false);
  if (await early.locator('#show-codes').count()) throw new Error('QR codes offered on a computer');
  await early.locator('#cancel-order').click();
  await early.locator('#cancel-order').waitFor({ state: 'detached', timeout: 10000 });
  if (await early.locator('#cancel-confirm').count()) throw new Error('asked to confirm a cancel before the address was shown');
  await early.locator('.tab[data-tab="history"]').click();
  await early.locator('.history-item', { hasText: 'cancelled' }).waitFor({ timeout: 5000 });
  await checkNoErrors(early);
  await early.context().close();
  step('a swap cancelled before its recovery file is checked goes without a warning');

  // A phone whose browser offers only the camera and photos: the recovery goes out as QR codes, and a screenshot of one reads back,
  // both to unlock the next step and in Rescue, on a browser that has never seen the swap.
  // A coarse pointer, while test.coarse is set: the test wallet connects as on a computer first.
  const asPhone = (page) => page.addInitScript(() => {
    const real = window.matchMedia.bind(window);
    window.matchMedia = (q) => real(q === '(pointer: coarse)' && localStorage.getItem('test.coarse') ? 'all' : q);
  });
  // A phone screen's pixel density, so the screenshot is as sharp as a real one.
  const phoneScreen = { width: 400, height: 860, deviceScaleFactor: 3 };
  const phoneish = await newPage(phoneScreen, asPhone);
  await upToRecovery(phoneish, false);
  await phoneish.evaluate(() => localStorage.setItem('test.coarse', '1'));
  await phoneish.reload();
  await phoneish.locator('#show-codes').click({ timeout: 30000 });
  // It opens full screen at once, which is what a person screenshots.
  const code = phoneish.locator('.qr-full > img');
  await code.waitFor({ timeout: 10000 });
  // Above the code, what the swap is, so a screenshot can be told apart from others later.
  const details = await phoneish.locator('.qr-full-details').innerText();
  if (!/L-USDt → tUSDT0 · You send .+ L-USDt, you receive .+ tUSDT0/.test(details) || !/Recovery for swap \w{8} · /.test(details)) throw new Error(`the details read: ${details}`);
  await phoneish.screenshot({ path: path.join(shots, 'recovery-code-full.png') });
  const shot = path.join(shots, `recovery-code-${Date.now()}.png`);
  await code.screenshot({ path: shot });
  await phoneish.keyboard.press('Escape');
  await phoneish.locator('.qr-full').waitFor({ state: 'detached', timeout: 5000 });
  await phoneish.locator('.recovery-code canvas').waitFor({ timeout: 5000 });
  await phoneish.screenshot({ path: path.join(shots, 'recovery-codes-phone.png'), fullPage: true });
  if (await phoneish.locator('.pay-address').count()) throw new Error('showing the codes unlocked the next step');
  await phoneish.locator('#recovery-file').setInputFiles(shot);
  await phoneish.locator('.pay-address').waitFor({ timeout: 15000 });
  step('on a phone, the recovery shows as a QR code, and its screenshot unlocks the next step');
  const elsewhere = await newPage(phoneScreen, (p) => asPhone(p).then(() => p.addInitScript(() => localStorage.setItem('test.coarse', '1'))));
  await elsewhere.locator('#burger').click();
  await elsewhere.locator('.tab[data-tab="rescue"]').click();
  await elsewhere.locator('#rescue-file').setInputFiles(shot);
  await elsewhere.locator('.swap-card[data-state="fund"]').waitFor({ timeout: 30000 });
  const [here, there] = await Promise.all([phoneish, elsewhere].map((p) => p.locator('.pay-address').innerText()));
  if (here !== there) throw new Error(`the screenshot restored another address: ${there} for ${here}`);
  await elsewhere.locator('#burger').click();
  await elsewhere.locator('.tab[data-tab="rescue"]').click();
  await elsewhere.locator('#rescue-file').setInputFiles({ name: 'cat.png', mimeType: 'image/png', buffer: Buffer.from(await phoneish.screenshot({ clip: { x: 0, y: 0, width: 200, height: 120 } })) });
  await elsewhere.locator('#banner:not([hidden])', { hasText: 'No QR code could be read' }).waitFor({ timeout: 10000 });
  await checkNoErrors(phoneish);
  await Promise.all([phoneish, elsewhere].map((p) => p.context().close()));
  step('Rescue takes the screenshot on a browser that never saw the swap, derives the same address, and refuses a picture without a code');

  const lock = await newPage();
  await startSwap(lock, true, await walletCli('getnewaddress'));
  await lock.locator('.lock').waitFor({ timeout: 10000 });
  await lock.route((url) => url.href.startsWith(RPC), async (route) => {
    const body = route.request().postDataJSON();
    if (!(body?.method === 'eth_call' && body.params[0].data.startsWith(LOCKS))) return route.fallback();
    return route.fulfill({ json: { jsonrpc: '2.0', id: body.id, result: openLock } });
  });
  await confirm(lock);
  await refused(lock, 'already been locked', 'lock');
  await lock.unrouteAll();
  await confirm(lock);
  await lock.locator('.swap-card[data-state="lock"]').waitFor({ state: 'detached', timeout: 10000 });
  await lock.context().close();
  step('P to L cancels too, unless its lock exists');
}

// The maker locks as soon as it has the page's signature, so the page's messages are held back until it asks for its refund.
// A maker that signed never locks, and the coin goes back on the key path, looking like any claim.
async function early() {
  let holding = false;
  const page = await newPage(undefined, (p) => p.routeWebSocket(/.*/, (ws) => {
    const relay = ws.connectToServer();
    ws.onMessage((m) => { if (!(holding && String(m).startsWith('["EVENT"'))) relay.send(m); });
    relay.onMessage((m) => ws.send(m));
  }));
  await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  holding = true;
  await pay(await page.locator('.pay-address').innerText(), amountSats(env.AMOUNT), env.USDT);
  await page.locator('.early-refund').waitFor({ timeout: 120000 });
  // The page's answer to the claim offer never reached the maker, so it can't lock. The cancel goes out at once.
  holding = false;
  await page.locator('.early-refund').click();
  await page.locator('.swap-card[data-state="early_form"] .maker-cancelled').waitFor({ timeout: 60000 });
  step('the maker cancelled at once, before any fee coin or address');
  if (!(await page.locator('.early-go').isDisabled())) throw new Error('the refund can be asked for with no fee coin');
  const fee = await page.locator('.fee-address').innerText();
  await pay(fee, 30, env.LBTC);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  const to = await walletCli('getnewaddress');
  await page.locator('.refund-to').fill(to);
  await page.locator('.fee-address').click();
  await page.locator('#toast', { hasText: 'Address copied' }).waitFor({ timeout: 2000 });
  if ((await page.evaluate(() => navigator.clipboard.readText())) !== fee) throw new Error('the fee address copied wrong');
  await page.locator('.fee-status', { hasText: 'Ready' }).waitFor({ timeout: 60000 });
  await page.locator('body').click({ position: { x: 5, y: 5 } });
  await new Promise((r) => setTimeout(r, 2500));
  if ((await page.locator('.refund-to').inputValue()) !== to) throw new Error('the refund address was lost when the page redrew');
  step('the refund form keeps the address through redraws, copies the fee address, and turns on once the fee coin is confirmed');
  await page.locator('.early-go:not([disabled])').click({ timeout: 10000 });
  step('asked for a refund before the maker locked');
  await page.locator('.swap-card[data-state="refunded"]').waitFor({ timeout: 120000 });
  const swap = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps')))[0]);
  const refundLink = page.locator('.swap-card .refund-links a.explorer-spend', { hasText: 'Liquid refund' });
  await refundLink.waitFor({ timeout: 30000 });
  if (!(await refundLink.getAttribute('href')).includes(`/tx/${swap.refund_txid}`)) throw new Error('the refunded card links to another transaction');
  const tx = JSON.parse(await cli('getrawtransaction', swap.refund_txid, '1'));
  if (tx.vin[0].txinwitness.length !== 1) throw new Error(`the refund's witness has ${tx.vin[0].txinwitness.length} items, so it shows it was a refund`);
  const got = await receivedAt(to);
  if (got !== Number(swap.terms.liquid_amount)) throw new Error(`refunded ${got} units, expected ${swap.terms.liquid_amount}`);
  step('the maker agreed, and the refund paid it all back, as one signature, long before the lock height');
  await checkNoErrors(page);
  await page.context().close();
}

// The page's view of Polygon's head, made to run behind by `offset.secs`, a block a second, so it locks with deadlines too near for
// the maker's gate.
async function headBehind(page, offset) {
  await page.route((url) => url.href.startsWith(RPC), async (route) => {
    const body = route.request().postDataJSON();
    if (body?.method !== 'eth_getBlockByNumber' || body.params[0] !== 'latest' || !offset.secs) return route.fallback();
    const json = await (await route.fetch()).json();
    const back = (hex) => `0x${(BigInt(hex) - BigInt(offset.secs)).toString(16)}`;
    return route.fulfill({ json: { ...json, result: { ...json.result, number: back(json.result.number), timestamp: back(json.result.timestamp) } } });
  });
}

const storedSwap = (page) => page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps')))[0]);

// P→L before the maker locks. The maker won't lock with these deadlines, so only the page's request hands the lock back.
// First, Send max stops at what the wallet holds.
async function release() {
  const poor = await newPage();
  const holds = BigInt(Math.round(Number(env.AMOUNT) * 1e6));
  await poor.route((url) => url.href.startsWith(RPC), async (route) => {
    const body = route.request().postDataJSON();
    if (!(body?.method === 'eth_call' && body.params[0].data.startsWith(BALANCE_OF))) return route.fallback();
    return route.fulfill({ json: { jsonrpc: '2.0', id: body.id, result: `0x${holds.toString(16).padStart(64, '0')}` } });
  });
  await connectTestWallet(poor);
  await poor.locator('.flip').click();
  await poor.locator('tbody tr').first().waitFor({ timeout: 30000 });
  const max = poor.locator('#send-max');
  await poor.locator('#send-max[title^="All the tUSDT0 this wallet holds"]').waitFor({ timeout: 30000 });
  if (Number((await max.innerText()).replace(/[^0-9.]/g, '')) !== Number(env.AMOUNT)) throw new Error(`Send max shows ${await max.innerText()}`);
  await poor.locator('.flip').click();
  if ((await max.getAttribute('title')).startsWith('All the tUSDT0')) throw new Error('L to P is capped by the Polygon wallet');
  await checkNoErrors(poor);
  await poor.context().close();
  step('P to L\'s Send max stops at the wallet\'s tUSDT0, and L to P\'s doesn\'t');

  const offset = { secs: 0 };
  const page = await newPage();
  await headBehind(page, offset);
  await startSwap(page, true, await walletCli('getnewaddress'));
  await page.locator('.lock').waitFor({ timeout: 10000 });
  // The page adds the maker's minimum and 300 s to the head it reads, so the real deadlines land 60 s out.
  offset.secs = Number((await storedSwap(page)).terms.polygon_window_secs) + 300 - 60;
  await page.locator('.lock').click();
  await page.locator('.swap-card[data-state="await_maker"] .early-refund').waitFor({ timeout: 30000 });
  step('while the maker hasn\'t locked on Liquid, P to L offers to ask for a refund');
  // The maker reads the lock only once it is final, and says no before that, as it should.
  // The banner may still show the bad recovery file from the start.
  const earlier = await page.locator('#banner').innerText().catch(() => '');
  for (let i = 0; i < 20 && (await state(page)) !== 'refunded'; i++) {
    if (await page.locator('.swap-card[data-state="await_maker"] .early-refund').count()) await page.locator('.early-refund').click();
    await waitState(page, 'refunded', 5000).catch(() => {});
    // A maker turning it down only changes the status line. A banner means the request itself went wrong.
    const banner = page.locator('#banner:not([hidden])');
    if (await banner.count() && (await banner.innerText()) !== earlier) throw new Error(`asking for the release failed: ${await banner.innerText()}`);
  }
  await waitState(page, 'refunded', 5000);
  const lockLink = page.locator('.swap-card .refund-links a.explorer-polygon', { hasText: 'Polygon lock' });
  await lockLink.waitFor({ timeout: 5000 });
  if (!(await lockLink.getAttribute('href')).endsWith(`/tx/${(await storedSwap(page)).lock_hash}`)) throw new Error('the refunded card links somewhere else');
  step('the maker agreed, and the lock came back on Polygon, with the lock linked on the card');
  await checkNoErrors(page);
  await page.context().close();
}

// A coin of the wrong amount is still the taker's. Past the lock height it comes back by refund, and the swap never expires,
// even for a swap an older page had already marked expired.
async function wrong() {
  const page = await newPage();
  await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  const paid = amountSats(env.AMOUNT) - 100;
  await pay(await page.locator('.pay-address').innerText(), paid, env.USDT);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  await page.locator('.swap-card[data-state="fund"] .warn', { hasText: 'wrong asset or amount' }).waitFor({ timeout: 60000 });
  const swap = await storedSwap(page);
  await walletCli('generatetoaddress', String(Math.max(0, swap.terms.lock_height - Number(await cli('getblockcount')) + 1)), await walletCli('getnewaddress'));
  await waitState(page, 'refund', 60000);
  step('past the lock height, a swap holding the wrong coin goes to a refund, not to expired');

  await page.evaluate((id) => {
    const all = JSON.parse(localStorage.getItem('liq2any.swaps'));
    all[id].state = 'expired';
    localStorage.setItem('liq2any.swaps', JSON.stringify(all));
  }, swap.id);
  await page.reload();
  for (let i = 0; i < 60 && (await storedSwap(page)).state !== 'refund'; i++) await new Promise((r) => setTimeout(r, 500));
  if ((await storedSwap(page)).state !== 'refund') throw new Error(`an expired swap holding a coin stayed ${(await storedSwap(page)).state}`);
  step('a swap an older page marked expired, with a coin at its address, goes back to a refund');

  await page.locator('.tab[data-tab="history"]').click();
  await page.locator('.history-item').first().locator('.history-row').click();
  await page.getByRole('button', { name: 'Open' }).first().click();
  await waitState(page, 'refund', 10000);
  const note = await page.locator('.refund-asset:not([hidden])').innerText({ timeout: 15000 });
  if (!note.includes(`${(paid / 1e8).toFixed(8).replace(/\.?0+$/, '')} USDt`) || !note.includes('not the swap')) throw new Error(`the refund says: ${note}`);
  step('the refund says it sends back the coin that arrived, not the swap\'s amount');
  if (!(await page.locator('.refund').isDisabled())) throw new Error('Refund is on with no fee coin');
  await pay(await page.locator('.fee-address').innerText(), 30, env.LBTC);
  // The suite mines every 3 seconds and its stand-in Esplora sees only confirmed coins, so an unconfirmed fee coin can't be caught here.
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  const back = await walletCli('getnewaddress');
  await page.locator('.fee-status', { hasText: 'Ready' }).waitFor({ timeout: 60000 });
  await page.locator('.refund:not([disabled])').waitFor({ timeout: 10000 });
  step('Refund is off with no fee coin, and on once it is confirmed');
  await page.locator('.refund-to').fill(back);
  await page.locator('.refund').click();
  await waitState(page, 'refunded', 120000);
  const got = await receivedAt(back);
  if (got !== paid) throw new Error(`the refund paid ${got} units, expected ${paid}`);
  step('the wrong coin came back in full');
  await checkNoErrors(page);
  await page.context().close();
}

// A coin of the wrong amount comes back early too: the maker cancels at once, and signs its refund.
async function wrongEarly() {
  const page = await newPage();
  await startSwap(page, false);
  await page.locator('.pay-address').waitFor();
  const paid = amountSats(env.AMOUNT) - 100;
  await pay(await page.locator('.pay-address').innerText(), paid, env.USDT);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  await page.locator('.swap-card[data-state="fund"] .early-refund').click({ timeout: 60000 });
  await page.locator('.swap-card[data-state="early_form"] .maker-cancelled').waitFor({ timeout: 60000 });
  step('with only a wrong coin paid, the page offers an early refund, and the maker cancels');
  const note = await page.locator('.refund-asset:not([hidden])').innerText({ timeout: 15000 });
  if (!note.includes('not the swap')) throw new Error(`the refund says: ${note}`);
  await pay(await page.locator('.fee-address').innerText(), 30, env.LBTC);
  await walletCli('generatetoaddress', '1', await walletCli('getnewaddress'));
  const back = await walletCli('getnewaddress');
  await page.locator('.refund-to').fill(back);
  await page.locator('.early-go:not([disabled])').click({ timeout: 60000 });
  await waitState(page, 'refunded', 120000);
  const got = await receivedAt(back);
  if (got !== paid) throw new Error(`the refund paid ${got} units, expected ${paid}`);
  const swap = await storedSwap(page);
  if (Number(await cli('getblockcount')) >= swap.terms.lock_height) throw new Error('the refund waited for the lock height');
  step('the wrong coin came back in full, long before the lock height');
  await checkNoErrors(page);
  await page.context().close();
}

const LOCKS = '0xed5e8437';
const BALANCE_OF = '0x70a08231';
const word = (hex) => hex.replace(/^0x/, '').padStart(64, '0');
// What `locks(id)` returns for an open lock: eleven words, the third its state.
const openLock = `0x${word('1')}${word('ffffffff')}${word('1')}${'0'.repeat(64 * 8)}`;

const tokenBalance = async (who) => BigInt(await polygonRpc('eth_call', [{ to: env.TOKEN, data: `${BALANCE_OF}${word(who)}` }, 'latest']));


// The stand-in token's own faucet, with a wallet that has never used it.
async function faucet() {
  const broke = await freshWallet(false);
  const noPol = await newPage(undefined, null, false, broke);
  await noPol.goto(`http://127.0.0.1:${site.port}/#faucet`);
  await noPol.locator('#sideswap-note', { hasText: 'SideSwap' }).waitFor({ timeout: 15000 });
  if (!(await noPol.locator('.card.faucet .addr').innerText()).includes(env.USDT)) throw new Error('the page should name the Liquid asset');
  await noPol.locator('#faucet-connect').click();
  await noPol.locator('#drip').click();
  await noPol.locator('#banner:not([hidden])', { hasText: 'test POL first' }).waitFor({ timeout: 10000 });
  await noPol.context().close();

  const who = await freshWallet();
  const page = await newPage(undefined, null, false, who);
  await page.goto(`http://127.0.0.1:${site.port}/#faucet`);
  await page.locator('#faucet-connect').click();
  // A wallet that never answers: Stop waiting frees the page.
  await page.evaluate(() => localStorage.setItem('test.signDelay', '600000'));
  await page.locator('#drip').click();
  await page.locator('#wallet-stop').click({ timeout: 15000 });
  await page.locator('#banner:not([hidden])', { hasText: 'Stopped waiting for Test Wallet' }).waitFor({ timeout: 5000 });
  await page.locator('#wallet-wait').waitFor({ state: 'detached', timeout: 5000 });
  step('a wallet that never answers can be given up on, and the page says approving later still sends it');
  await page.reload();
  await page.locator('#faucet-connect, #drip').first().waitFor({ timeout: 15000 });
  if (await page.locator('#faucet-connect').count()) await page.locator('#faucet-connect').click();
  await page.evaluate(() => localStorage.setItem('test.signDelay', '2000'));
  await page.locator('#drip').click();
  const waiting = page.locator('#wallet-wait', { hasText: 'Approve in Test Wallet' });
  await waiting.waitFor({ timeout: 15000 });
  if (!/in its own window/.test(await waiting.innerText())) throw new Error('the approval note doesn\'t say where to look');
  await waiting.waitFor({ state: 'detached', timeout: 15000 });
  await page.evaluate(() => localStorage.removeItem('test.signDelay'));
  step('while the wallet has a transaction to approve, the page says so, and the note goes once it is sent');
  await page.locator('#faucet-balance', { hasText: '100 tUSDT0' }).waitFor({ timeout: 60000 });
  if ((await tokenBalance(who.address)) !== 100000000n) throw new Error('the faucet gave another amount');
  await checkNoErrors(page);
  await page.context().close();
  step('a new wallet gets its test tUSDT0 from the stand-in token, and one with no POL is told so');
}

// A maker that stops publishing has to drop off a page that's already open. Moving the page's clock past every offer's
// expiry looks the same as all of them going quiet.
async function deadMakers() {
  const page = await newPage(undefined, (p) => p.clock.install());
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.clock.fastForward(5 * 60 * 1000);
  await page.locator('tbody tr').first().waitFor({ state: 'detached', timeout: 15000 });
  await page.context().close();
  step('a maker that stops publishing drops off a page that is already open');
}

// A quote counts down, and once its price lapses the page offers a new one instead of an accept the maker would refuse.
// The suite's maker holds a price for 4 s.
async function quoteExpiry() {
  const page = await newPage();
  await connectTestWallet(page);
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.locator('#amount').fill(env.AMOUNT);
  await page.locator('#go:not(:disabled)').click();
  await page.locator('#accept').waitFor({ timeout: 30000 });
  const first = await page.locator('#quote-left').textContent();
  await page.waitForFunction((was) => document.getElementById('quote-left')?.textContent !== was, first, { timeout: 3000 });
  await page.locator('#requote').waitFor({ timeout: 10000 });
  if (await page.locator('#accept').count() || (await page.locator('#quote-left').textContent()) !== 'Expired') throw new Error('an expired price can still be accepted');
  await page.locator('#requote').click();
  await page.locator('#accept').waitFor({ timeout: 30000 });
  await page.locator('.btn.quiet', { hasText: 'Cancel' }).click();
  await checkNoErrors(page);
  await page.context().close();
  step('a quote counts down, and an expired price offers a new one instead of Accept');
}

// The network switch. The suite's chains aren't mainnet, so a page switched to mainnet must refuse to run, which is the guard
// that matters: mainnet's name never goes with other chains. Mainnet has no program yet, so one page is told it has.
// The published page pins every file by hash. It runs as published, and refuses a changed script or WebAssembly.
async function pinnedFiles() {
  const open = async (tamper) => {
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
    const requests = [];
    page.on('request', (r) => requests.push(r.url()));
    if (tamper) await page.route(tamper.url, async (route) => {
      const body = Buffer.from(await (await route.fetch()).body());
      await route.fulfill({ body: tamper.change(body), contentType: tamper.type });
    });
    await page.goto(`http://127.0.0.1:${site.port}/stamped/`);
    return { page, context, errors, requests };
  };
  const plain = await open(null);
  await plain.page.locator('html[data-network]').waitFor({ state: 'attached', timeout: 30000 });
  if (plain.errors.some((e) => /integrity|digest|WebAssembly/i.test(e))) throw new Error(`the stamped page reported: ${plain.errors}`);
  // Every file is asked for at an address carrying its hash, so no copy cached from an earlier build can be paired with this one.
  const unversioned = plain.requests.filter((u) => /\/stamped\/.*\.(js|css|wasm)(\?|$)/.test(u) && !/\?v=[0-9a-f]{16}$/.test(u));
  if (unversioned.length) throw new Error(`files asked for without their version: ${unversioned}`);
  await plain.context.close();

  const script = await open({ url: '**/stamped/js/plan.js?v=*', type: 'text/javascript', change: (b) => Buffer.concat([b, Buffer.from('\n// changed\n')]) });
  await new Promise((r) => setTimeout(r, 3000));
  if (await script.page.locator('html[data-network]').count()) throw new Error('the page ran with a changed script');
  if (!script.errors.some((e) => /integrity/i.test(e))) throw new Error(`no integrity error for a changed script: ${script.errors}`);
  await script.context.close();

  const wasm = await open({ url: '**/stamped/wasm/*.wasm?v=*', type: 'application/wasm', change: (b) => Buffer.concat([b, Buffer.from([0, 0])]) });
  await wasm.page.locator('#banner:not([hidden])', { hasText: "doesn't match its build" }).waitFor({ timeout: 30000 });
  if (await wasm.page.locator('html[data-network]').count()) throw new Error('the page ran with changed WebAssembly');
  await wasm.context.close();
  step('the published page runs as stamped, and refuses a changed script or WebAssembly');
}

// On mainnet, the menu's sol2base and eth2base links open in this tab, except while a swap is unfinished, when they open a new one
// so this tab keeps the swap going.
async function sisterLinks() {
  for (const state of [null, 'fund', 'done']) {
    const page = await newPage({ width: 1200, height: 1000 }, async (p) => {
      await mainnetConfig(p);
      await p.context().route('https://swapmarket.github.io/sol2base/', (r) => r.fulfill({ body: '<title>sol2base</title>', contentType: 'text/html' }));
      await p.addInitScript((swaps) => {
        if (!sessionStorage.getItem('seen')) {
          sessionStorage.setItem('seen', '1');
          localStorage.setItem('test.mainnet', '1');
          if (swaps) localStorage.setItem('liq2any.swaps.mainnet', swaps);
        }
      }, state && JSON.stringify({ x: { id: 'x', state } }));
    });
    await page.locator('html[data-network="mainnet"]').waitFor({ state: 'attached', timeout: 30000 });
    const opened = page.context().waitForEvent('page', { timeout: 5000 }).catch(() => null);
    await page.locator('a.tab', { hasText: 'sol2base' }).click();
    const other = await opened;
    const unfinished = state === 'fund';
    if (unfinished && (!other || await page.title() !== 'liq2any')) throw new Error('with a swap unfinished, sol2base did not open in a new tab');
    if (!unfinished) {
      await page.waitForURL('https://swapmarket.github.io/sol2base/', { timeout: 10000 });
      if (other) throw new Error(`with ${state ?? 'no'} swap, sol2base opened a new tab too`);
    }
    await page.context().close();
  }
  step('sol2base opens in this tab, or in a new one while a swap is unfinished');
}

// Framed by another site, the page runs nothing and only offers itself in its own tab, so an overlay can't steer clicks on it.
async function refusesFrames() {
  const other = await serve((req, res) => {
    res.setHeader('content-type', 'text/html');
    res.end(`<!doctype html><iframe src="http://127.0.0.1:${site.port}/" width="800" height="600"></iframe>`);
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(`http://127.0.0.1:${other.port}/`);
  const frame = page.frameLocator('iframe');
  await frame.locator('a', { hasText: 'Open it in its own tab' }).waitFor({ timeout: 30000 });
  await new Promise((r) => setTimeout(r, 2000));
  if (await frame.locator('html[data-network]').count() || await frame.locator('#amount').count()) throw new Error('the page ran inside a frame');
  if (await frame.locator('a', { hasText: 'Open it in its own tab' }).getAttribute('target') !== '_blank') throw new Error('the link opens inside the frame');
  await context.close();
  other.server.close();
  step('inside another site\'s frame, the page runs nothing and offers itself in its own tab');
}

// The test config fills in these chains' endpoints over whichever network was chosen. On mainnet it keeps mainnet's name, so the
// page holds these test chains to mainnet, and refuses them.
const mainnetConfig = (p) => p.route('**/js/config.js', async (route) => {
  const body = `${await (await route.fetch()).text()}\nif (localStorage.getItem('test.mainnet')) Object.assign(NETWORK, { name: 'mainnet', regtest: false, faucet: null, label: 'Liquid + Polygon mainnet' });\n`;
  await route.fulfill({ body, contentType: 'text/javascript' });
});

async function networkSwitch() {
  // No escrow on mainnet yet, so Settings can't switch to it.
  const page = await newPage();
  await page.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await page.locator('.star').first().click();
  await page.locator('.star.on').first().waitFor({ timeout: 3000 });
  await openSettings(page);
  if (!(await page.locator('#switch-network').isDisabled())) throw new Error('mainnet can be chosen with no escrow there');
  if (!/not available yet/.test(await page.locator('#network-note').innerText())) throw new Error('no reason given for the switch being off');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await page.context().close();
  step('with no escrow on mainnet, Settings says so and can\'t switch to it');

  // A page put on mainnet's name, against these test chains, refuses to run, and keeps its own storage.
  const real = await newPage({ width: 1200, height: 1000 }, async (p) => {
    await mainnetConfig(p);
    await p.addInitScript(() => localStorage.setItem('test.mainnet', '1'));
  });
  await real.locator('html[data-network="mainnet"]').waitFor({ state: 'attached', timeout: 30000 });
  if (await real.locator('#net').isVisible()) throw new Error('mainnet shows a network badge');
  await real.locator('#banner:not([hidden])', { hasText: 'set to mainnet, and an endpoint is on another network' }).waitFor({ timeout: 30000 });
  if (await real.locator('.tab[data-tab="faucet"]').isVisible()) throw new Error('the faucet shows on mainnet');
  for (const name of ['', 'liq2sol/', 'sol2base/', 'eth2base/']) {
    const link = real.locator(`a.tab[href="https://swapmarket.github.io/${name}"]`);
    if (!(await link.isVisible()) || (await link.getAttribute('target')) !== '_blank') throw new Error(`no link to swapmarket.github.io/${name} on mainnet`);
  }
  if (await real.locator('tbody tr').count()) throw new Error('offers show on a page that refused to run');
  // Its log, written at start, goes under mainnet's own name, and nothing under the test network's.
  const kept = await real.evaluate(() => [localStorage.getItem('liq2any.logs.mainnet'), localStorage.getItem('liq2any.logs'), localStorage.getItem('liq2any.settings'), localStorage.getItem('liq2any.swaps')]);
  if (!kept[0]?.includes('started') || kept.slice(1).some((v) => v !== null)) throw new Error(`mainnet shares storage with the test network: ${kept.map((v) => v?.slice(0, 40))}`);
  await real.context().close();
  step('a page on mainnet\'s name refuses chains that aren\'t mainnet, hides the faucet, and keeps its own storage');
}

async function look() {
  await deadMakers();
  await quoteExpiry();
  await phoneWallets();
  await connectOrder();
  await p2lBalance();
  await walletStays();
  await pageLoading();
  await networkSwitch();
  await pinnedFiles();
  await refusesFrames();
  await sisterLinks();
  await walletConnectWait();
  await liveBook();
  const phone = await newPage({ width: 400, height: 800 });
  await phone.locator('tbody tr').first().waitFor({ timeout: 30000 });
  await phone.locator('#amount').fill(env.AMOUNT);
  await checkSettingsLogs(phone);
  await phone.locator('.star').first().click();
  await phone.locator('.star.on').first().waitFor({ timeout: 3000 });
  await phone.locator('#only-favorites').check();
  await phone.locator('.star.on').first().waitFor({ timeout: 3000 });
  await phone.reload();
  await phone.locator('.star.on').first().waitFor({ timeout: 30000 });
  if (!(await phone.locator('#only-favorites').isChecked())) throw new Error('"only favorites" did not survive a reload');
  await phone.locator('.star.on').first().click();
  await phone.locator('tbody tr').waitFor({ state: 'detached', timeout: 3000 });
  await phone.locator('#only-favorites').uncheck();
  await phone.locator('tbody tr').first().waitFor({ timeout: 3000 });
  step('a favorite and the switch survive a reload, and the switch hides everyone else');
  step('Settings can download and delete the logs');
  await phone.screenshot({ path: path.join(shots, 'phone.png'), fullPage: true });
  const wide = await phone.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (wide) throw new Error('the page scrolls sideways on a phone');
  await checkNoErrors(phone);
  await phone.context().close();
}

let code = 0;
try {
  for (const name of ['vetting', ...(env.BROWSER_SCENARIOS ?? 'look,cancelling,faucet,l2p,p2l,split,rescue,early,release,wrong,wrongEarly').split(',')]) {
    console.log(`  == browser: ${name}`);
    await { vetting, look, cancelling, faucet, l2p, p2l, rescue, split, early, release, wrong, wrongEarly }[name]();
  }
  console.log('BROWSER OK');
} catch (e) {
  console.log(`BROWSER FAILED: ${e.message}`);
  for (const context of browser.contexts()) {
    for (const page of context.pages()) {
      await page.screenshot({ path: path.join(shots, 'failed.png') }).catch(() => {});
      const seen = await page.locator('#banner').innerText({ timeout: 1000 }).catch(() => '');
      const card = await page.locator('.swap-card').first().getAttribute('data-state', { timeout: 1000 }).catch(() => '');
      console.log(`    the page shows state "${card}" and message "${seen.slice(0, 300)}"`);
      const swaps = await page.evaluate(() => Object.values(JSON.parse(localStorage.getItem('liq2any.swaps') ?? '{}'))).catch(() => []);
      for (const x of swaps) {
        console.log(`    swap ${x.secrets.lock_point.slice(0, 12)}: state ${x.state}, status "${x.status}", tip ${x.tip}, lock height ${x.terms.lock_height}, accepted at ${x.terms.h_accept}, coin ${JSON.stringify(x.coin ?? null)}, signed ${!!x.signed}, lock ${x.lock_id}`);
      }
    }
  }
  code = 1;
} finally {
  await browser.close();
  shim.server.close();
  site.server.close();
}
process.exit(code);
