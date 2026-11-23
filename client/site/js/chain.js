// Reading and writing both chains over public APIs. Nothing here decides anything: the WASM reads coins and accounts.
import * as W from '../wasm/liq2any_client_wasm.js';

export const hexToBytes = (h) => Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
export const bytesToHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');

// Each swap's steps run one at a time, so a request that never answers would hold up everything after it, a claim included.
const HTTP_TIMEOUT_MS = 30000;

async function http(url, options) {
  let res;
  try {
    res = await fetch(url, { ...options, signal: AbortSignal.timeout(HTTP_TIMEOUT_MS) });
  } catch (e) {
    if (e?.name === 'TimeoutError') throw new Error(`${new URL(url).host} didn't answer in ${HTTP_TIMEOUT_MS / 1000} s`);
    throw e;
  }
  const text = await res.text();
  if (!res.ok) throw new Error(`${new URL(url).host}: ${text.slice(0, 200) || res.status}`);
  return text;
}

export class Esplora {
  constructor(base) {
    this.base = base;
    this.cache = new Map();
  }

  tip = async () => Number(await http(`${this.base}/blocks/tip/height`));
  genesis = () => http(`${this.base}/block-height/0`).then((h) => h.trim());
  history = async (address) => JSON.parse(await http(`${this.base}/address/${address}/txs`));
  utxos = async (address) => JSON.parse(await http(`${this.base}/address/${address}/utxo`));

  // Fixed by its id, so one copy is enough, and it costs a public API only once.
  async txHex(txid) {
    if (!this.cache.has(txid)) this.cache.set(txid, (await http(`${this.base}/tx/${txid}/hex`)).trim());
    return this.cache.get(txid);
  }

  // The registry's entry for an asset: name, ticker, precision and the issuer's domain. Absent for unregistered ones.
  asset = async (id) => JSON.parse(await http(`${this.base}/asset/${id}`));

  broadcast = (hex) => http(`${this.base}/tx`, { method: 'POST', body: hex }).then((t) => t.trim());
  outspend = async (txid, vout) => JSON.parse(await http(`${this.base}/tx/${txid}/outspend/${vout}`));
  status = async (txid) => JSON.parse(await http(`${this.base}/tx/${txid}/status`));

  // Every output at this script that reads as what we were told, with its depth. Junk at the address is simply not in the list.
  async readCoins(plainAddress, script, blindingKey) {
    const tip = await this.tip();
    const found = [];
    for (const utxo of await this.utxos(plainAddress)) {
      const hex = await this.txHex(utxo.txid);
      for (const coin of JSON.parse(W.read_coins(hex, script, blindingKey))) {
        if (coin.vout !== utxo.vout) continue;
        const height = utxo.status?.block_height ?? null;
        found.push({ ...coin, txid: utxo.txid, hex, confirmations: height === null ? 0 : tip + 1 - height });
      }
    }
    return found;
  }

  parentsOf = async (hex) => Promise.all(JSON.parse(W.parents_of(hex)).map((txid) => this.txHex(txid)));
}

// How far back the block time is measured, as the maker measures it (maker/daemon/src/lib.rs, BLOCK_TIME_SPAN).
const BLOCK_TIME_SPAN = 2000;

const quantity = (hex) => BigInt(hex ?? '0x0');

// One Polygon JSON-RPC endpoint. Decisions are made at `finalized`, and a claim waits for two endpoints to agree.
export class Polygon {
  constructor(url) {
    this.url = url;
  }

  async rpc(method, params = []) {
    const body = JSON.stringify({ jsonrpc: '2.0', id: 1, method, params });
    const text = await http(this.url, { method: 'POST', headers: { 'content-type': 'application/json' }, body });
    const reply = JSON.parse(text);
    // An RPC's message can repeat the URL, which may hold the user's key.
    if (reply.error) throw new Error(`Polygon RPC: ${reply.error.message}`.split('?')[0]);
    return reply.result;
  }

  chainId = async () => Number(quantity(await this.rpc('eth_chainId')));

  async block(tag = 'finalized') {
    const b = await this.rpc('eth_getBlockByNumber', [typeof tag === 'number' ? `0x${tag.toString(16)}` : tag, false]);
    if (!b) throw new Error(`Polygon has no ${tag} block yet`);
    return { number: Number(quantity(b.number)), time: Number(quantity(b.timestamp)) };
  }

  // Milliseconds per block over the last 2,000 finalized ones: how a deadline in blocks turns into time.
  async blockMs() {
    const f = await this.block('finalized');
    const past = await this.block(Math.max(0, f.number - BLOCK_TIME_SPAN));
    return Math.floor(((f.time - past.time) * 1000) / Math.max(1, f.number - past.number));
  }

  call = (to, data, tag = 'latest') => this.rpc('eth_call', [{ to, data }, tag]);

  // A lock as the escrow stores it, or null if there is none.
  async lock(escrow, id, tag = 'finalized') {
    const l = JSON.parse(W.parse_lock(await this.call(escrow, W.locks_call(id), tag)));
    return l.state === 'none' ? null : l;
  }

  tokenBalance = async (token, who, tag = 'latest') => quantity(await this.call(token, W.token_call('balance', who, ''), tag));
  allowance = async (token, owner, spender) => quantity(await this.call(token, W.token_call('allowance', owner, spender)));
  isBlocked = async (token, who) => quantity(await this.call(token, W.token_call('blocked', who, ''))) !== 0n;
  balance = async (who) => quantity(await this.rpc('eth_getBalance', [who, 'latest']));

  // A failed transaction still costs its fee and shows on-chain, so a claim isn't sent unless this comes back clean.
  async simulate({ from, to, data, value = '0x0' }) {
    try {
      await this.rpc('eth_call', [{ from, to, data, value }, 'latest']);
    } catch (e) {
      throw new Error(`The transaction wouldn't succeed, so it wasn't sent: ${e.message.replace('Polygon RPC: ', '')}`);
    }
  }

  // The transaction's outcome once mined, or null while it isn't.
  async receipt(hash) {
    const r = await this.rpc('eth_getTransactionReceipt', [hash]);
    return r ? { success: r.status === '0x1', block: Number(quantity(r.blockNumber)) } : null;
  }
}
