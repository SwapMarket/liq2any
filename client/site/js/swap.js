// One swap's state machine, from docs/protocol.md sections 4 and 5. It is resumed from storage after a reload, and each tick reads
// the chains again, so nothing depends on this page having stayed open. Decisions about funds and secrets are in the WASM.
import * as W from '../wasm/liq2any_client_wasm.js';
import { NETWORK, confirmationsFor } from './config.js';
import { log } from './logs.js';
import { saveSwap } from './store.js';

// USDT units of 10^-6 as text, trailing zeros dropped: 100000 is "0.1".
const showUnits = (u) => {
  const v = BigInt(u);
  const frac = (v % 1000000n).toString().padStart(6, '0').replace(/0+$/, '');
  return `${v / 1000000n}${frac ? `.${frac}` : ''}`;
};

const now = () => Math.floor(Date.now() / 1000);

export const rulesJson = (maxSpread = NETWORK.maxSpreadBps) => JSON.stringify({
  network: NETWORK.name, usdt_asset: NETWORK.usdt, chain_id: NETWORK.chainId, escrow: NETWORK.escrow, token: NETWORK.token,
  max_spread_bps: maxSpread, ...NETWORK.rules,
});

export const FINISHED = ['done', 'refunded', 'expired', 'cancelled'];
// Nothing has been paid or locked in these, as far as this page knows. cancelProblem asks the chains.
export const CANCELLABLE = ['recovery', 'fund', 'lock'];

// The step a swap is at, for its card: its state, except that waiting for Liquid confirmations is a step of its own. L→P waits
// for the payment's before the maker locks, and P→L for the maker's lock's before signing the claim.
export function phase(s) {
  if (s.state !== 'await_maker') return s.state;
  if (s.direction === 'L2P') return (s.coin?.confirmations ?? 0) < confirmationsFor(s.terms) ? 'confirming' : 'await_maker';
  return s.maker_locked ? 'maker_confirms' : 'await_maker';
}

const swapArgs = (s) => ({
  direction: s.direction, maker_key: s.terms.maker_key, taker_key: s.secrets.taker_key,
  blinding_key: s.secrets.blinding_key, lock_height: s.terms.lock_height,
});

/** Everything derived from agreed terms. The addresses are never taken from the maker. */
export function deriveSwap(s) {
  s.info = JSON.parse(W.swap_info(JSON.stringify(swapArgs(s))));
  // The escrow's id for the lock: by whoever makes it, and the address of our point.
  const initiator = s.direction === 'L2P' ? s.terms.maker_polygon : s.polygon_owner;
  s.lock_id = W.lock_id(initiator, s.secrets.lock_address);
  return s;
}

// How long a lock stays claimable, in seconds: a short leg until either clock runs out, a long leg until both have.
export function secsLeft(lock, block, blockMs) {
  const byTime = lock.deadline_time - block.time;
  const byBlocks = Math.floor(((lock.deadline_block - block.number) * blockMs) / 1000);
  return lock.long_leg ? Math.max(byTime, byBlocks) : Math.min(byTime, byBlocks);
}

// The escrow's own rule: whether a claim would still be accepted at this block.
const claimable = (lock, block) => {
  const time = block.time < lock.deadline_time;
  const blocks = block.number < lock.deadline_block;
  return lock.state === 'open' && (lock.long_leg ? time || blocks : time && blocks);
};

const same = (a, b) => !!a && !!b && a.toLowerCase() === b.toLowerCase();
const hexValue = (n) => `0x${BigInt(n).toString(16)}`;

// Elements' refusals of a transaction it already has, in a block or in its mempool.
const ALREADY_SENT = /already in block chain|already.*mempool|txn-already/i;

const FINISHED_EARLIER = 'This swap already finished, so there is nothing left to do here.';

const EARLY_REFUND_WAIT_SECS = 60;

export class Engine {
  constructor({ esplora, polygon, secondPolygon, getWallet, relays, onChange, allowOneEndpoint, feeCoinSats }) {
    Object.assign(this, { esplora, polygon, secondPolygon, getWallet, relays, onChange, allowOneEndpoint, feeCoinSats });
    this.blockTime = null;
    this.sessions = new Map();
    this.pendingOffers = new Map();
    this.earlyRefunds = new Map();
    this.recheckedExpired = new Set();
  }

  save(s) {
    if (s.logged_state !== s.state) {
      log('info', `swap ${s.id.slice(0, 8)} ${s.direction}: ${s.logged_state ?? 'new'} -> ${s.state}`, s.status ?? '');
      s.logged_state = s.state;
    }
    // When each step began, for the timers on the swap's card. A step reached again keeps its first time.
    const step = phase(s);
    if (!s.step_at?.[step]) s.step_at = { ...s.step_at, [step]: Math.floor(Date.now() / 1000) };
    saveSwap(s);
    this.onChange(s);
  }

  set(s, fields) {
    Object.assign(s, fields);
    this.save(s);
  }

  send(s, body) {
    const { event } = JSON.parse(W.wrap_message(s.nostr.secret, s.maker, s.id, NETWORK.name, JSON.stringify(body), BigInt(now())));
    this.relays.publish(event);
  }

  // Coins at the swap's address that are exactly what was agreed. A wrong coin is reported, and never acted on.
  async exactCoins(esplora, s) {
    const coins = await esplora.readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    return coins.filter((c) => c.is_usdt && c.amount === s.terms.liquid_amount);
  }

  // The coin a refund sends back: the swap's own if it is there, or else whatever arrived.
  async refundCoin(s) {
    const coins = await this.esplora[0].readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    return coins.find((c) => c.is_usdt && c.amount === s.terms.liquid_amount) ?? coins[0] ?? null;
  }

  // The measured time per block, read again every ten minutes: how the block clock of a deadline turns into time.
  async blockMs() {
    if (!this.blockTime || Date.now() - this.blockTime.at > 600000) this.blockTime = { ms: await this.polygon.blockMs(), at: Date.now() };
    return this.blockTime.ms;
  }

  lock = (s, tag = 'finalized', polygon = this.polygon) => polygon.lock(NETWORK.escrow, s.lock_id, tag);

  // Wait for a transaction the wallet sent to be mined. Throws if it reverted.
  async mined(hash, what) {
    const start = Date.now();
    for (;;) {
      const r = await this.polygon.receipt(hash).catch(() => null);
      if (r) {
        if (!r.success) throw new Error(`The ${what} failed on Polygon, so nothing changed.`);
        return r;
      }
      if (Date.now() - start > 180000) throw new Error(`The ${what} hasn't been mined after three minutes. Check your wallet.`);
      await new Promise((ok) => setTimeout(ok, 2000));
    }
  }

  // Where an L→P swap waits for the maker: paid, or still at the payment step with only a wrong coin.
  waiting(s) {
    return s.direction === 'L2P' && !s.coin ? 'fund' : 'await_maker';
  }

  async wrongCoins(s) {
    const coins = await this.esplora[0].readCoins(s.info.plain_address, s.info.script, s.secrets.blinding_key);
    return coins.filter((c) => !(c.is_usdt && c.amount === s.terms.liquid_amount));
  }

  // Why the maker's lock isn't safe to claim, or ''. It must be exactly the agreed one, and leave time on both clocks.
  checkLock(l, s, block, blockMs) {
    if (!l || l.state !== 'open') return 'it is not open';
    if (l.amount !== s.terms.polygon_amount) return 'the amount is wrong';
    if (!same(l.lock_address, s.secrets.lock_address)) return 'it locks to another point';
    if (!same(l.recipient, s.polygon_owner)) return 'it pays someone else';
    if (!same(l.initiator, s.terms.maker_polygon)) return 'someone other than the maker made it';
    if (!claimable(l, block) || secsLeft(l, block, blockMs) < NETWORK.claimCutoffSecs) return 'it is too close to its deadline to safely reveal the secret';
    return '';
  }

  // Revealing the secret against a lock that isn't there would give the swap away, so the lock is confirmed on two endpoints first.
  async secondOpinionOnLock(s, blockMs) {
    const second = this.secondPolygon();
    if (!second) return this.allowOneEndpoint() ? '' : 'one-endpoint';
    return this.checkLock(await this.lock(s, 'finalized', second), s, await second.block('finalized'), blockMs);
  }

  // ---- Cancelling, before anything is paid or locked ----

  // Why this swap can't be cancelled, or ''. A payment the page hasn't noticed yet would otherwise end up hidden. Not being able
  // to read the chain throws, and that counts as a no too.
  async cancelProblem(s) {
    if (!CANCELLABLE.includes(s.state)) return 'it has already moved on';
    if (s.direction === 'L2P') {
      return (await this.esplora[0].history(s.info.plain_address)).length ? 'a payment has already reached its address' : '';
    }
    return (await this.lock(s, 'latest')) ? 'it has already been locked on Polygon' : '';
  }

  // ---- A swap picked up from its recovery file ----

  // The file is saved before anything happens, so it says nothing about how far the swap got. Only the chains can, and until
  // they have, no step that spends is offered: paying or locking again on a finished swap could lose the money.
  async tickRestored(s, tip) {
    try {
      this.set(s, { status: '', ...(await (s.direction === 'L2P' ? this.restoredL2P(s, tip) : this.restoredP2L(s, tip))) });
    } catch (e) {
      this.set(s, { status: `Can't read this swap's history yet, so nothing is offered: ${e.message.slice(0, 120)}` });
    }
  }

  async restoredL2P(s, tip) {
    const lock = await this.lock(s, 'latest');
    if (lock?.state === 'claimed') return { state: 'done', status: 'You were paid on Polygon. The maker can now collect the Liquid coin.' };
    if (!(await this.esplora[0].history(s.info.plain_address)).length) {
      return { state: tip > s.terms.lock_height - 200 ? 'expired' : 'fund' };
    }
    if ((await this.esplora[0].utxos(s.info.plain_address)).length) return { state: 'await_maker' };
    return { state: 'done', status: FINISHED_EARLIER };
  }

  async restoredP2L(s, tip) {
    // A lock is never removed from the escrow, so its state says everything.
    const lock = await this.lock(s, 'latest');
    if (lock?.state === 'claimed') return { state: 'done', status: FINISHED_EARLIER };
    if (lock?.state === 'refunded') return { state: 'refunded' };
    if (lock) return { state: 'await_maker' };
    return { state: tip > s.terms.h_accept + 15 ? 'expired' : 'lock' };
  }

  // ---- L to P ----

  async tickL2P(s, tip) {
    const block = await this.polygon.block('finalized');
    const lock = await this.lock(s);
    if (lock?.state === 'claimed' && s.state !== 'done') {
      this.set(s, { state: 'done', status: 'You were paid on Polygon. The maker can now collect the Liquid coin.' });
      return;
    }
    // The maker locked first, so it won't agree now, and the swap carries on as normal.
    if (['early_form', 'early_asked', 'early_signing'].includes(s.state) && lock) s.state = 'await_maker';
    if (s.state === 'early_asked' && now() - s.early_asked_at > EARLY_REFUND_WAIT_SECS) {
      s.state = this.waiting(s);
      s.status = 'The maker did not answer. Nothing has changed, and the timed refund still protects you.';
    }
    if (s.state === 'early_signing' && now() - s.early_asked_at > EARLY_REFUND_WAIT_SECS) {
      s.state = 'early_form';
      s.status = 'The maker did not answer. Ask again, or wait for the timed refund.';
    }
    if (s.state === 'fund' || s.state === 'await_maker') {
      const coins = await this.exactCoins(this.esplora[0], s);
      const wrong = await this.wrongCoins(s);
      s.wrong_coin = wrong.length > 0;
      const coin = coins[0];
      if (coin) {
        s.coin = { txid: coin.txid, vout: coin.vout, confirmations: coin.confirmations };
        if (s.state === 'fund') s.state = 'await_maker';
        await this.answerPendingOffer(s);
      }
    }
    if (s.state === 'await_maker' || s.state === 'ready_claim') {
      const blockMs = await this.blockMs();
      const problem = lock ? this.checkLock(lock, s, block, blockMs) : 'no lock yet';
      s.escrow_problem = lock && problem ? problem : '';
      if (!problem) {
        s.lock = lock;
        s.state = 'ready_claim';
        s.secs_left = secsLeft(lock, block, blockMs);
        s.status = '';
      } else if (s.state === 'ready_claim') {
        s.state = 'await_maker';
      }
    }
    if (!['done', 'refund_wait', 'refund', 'refunding'].includes(s.state)) {
      // The maker's gate leaves 390 blocks when it locks, so past that it won't. With no coin, there is nothing to get back either.
      const makerGone = tip > s.terms.lock_height - 380 && !lock;
      const refunded = lock?.state === 'refunded';
      // Only an empty address expires. A coin of the wrong asset or amount is still the taker's, and comes back by refund.
      if (s.state === 'fund' && tip >= s.terms.lock_height) s.state = (await this.esplora[0].utxos(s.info.plain_address)).length ? 'refund' : 'expired';
      // A maker that cancelled won't lock, so an early refund stays on offer until the timed one opens.
      else if (tip >= s.terms.lock_height || ((makerGone || refunded) && !s.maker_cancelled)) s.state = 'refund_wait';
    }
    if (s.state === 'refund_wait' && tip >= s.terms.lock_height) s.state = 'refund';
    if (s.state === 'refund' || s.state === 'refunding') await this.tickRefund(s);
    s.tip = tip;
    this.save(s);
  }

  async answerPendingOffer(s) {
    const offer = this.pendingOffers.get(s.id);
    // Asking to cancel, or cancelled: the answer is what would let the maker lock.
    if (!['fund', 'await_maker'].includes(s.state) || s.maker_cancelled) return;
    if (!offer || !s.coin || s.answered === offer.nonce) return;
    // Marked first, because a tick and an arriving message can both get here while this waits on the network.
    s.answered = offer.nonce;
    const claim = offer.tx;
    const args = {
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point,
      coin_tx: await this.esplora[0].txHex(s.coin.txid), coin_vout: s.coin.vout,
      claim_tx: claim, maker_nonce: offer.nonce, parents: await this.esplora[0].parentsOf(claim), genesis: NETWORK.genesis,
    };
    // A nonce is made and used inside this call, so answering twice only wastes a little work.
    const answer = JSON.parse(W.answer_claim_offer(JSON.stringify(args)));
    this.send(s, { type: 'claim-answer', body: { lock_point: s.secrets.lock_point, nonce: answer.nonce, share: answer.share } });
    this.save(s);
  }

  async claimOnPolygon(s) {
    const wallet = this.getWallet();
    if (!wallet) throw new Error('Connect a Polygon wallet to send the claim.');
    const blockMs = await this.blockMs();
    const lock = await this.lock(s);
    const problem = this.checkLock(lock, s, await this.polygon.block('finalized'), blockMs);
    if (problem) throw new Error(`The secret stays hidden: ${problem}.`);
    const second = await this.secondOpinionOnLock(s, blockMs);
    if (second) throw new Error(second === 'one-endpoint' ? 'Add a second Polygon endpoint in Settings, or allow one endpoint there.' : `The second endpoint disagrees: ${second}.`);
    // A claim pays a blocked address all the same, and the coins would be stuck there.
    if (await this.polygon.isBlocked(NETWORK.token, s.polygon_owner)) throw new Error('Tether has blocked the address this swap pays, so the secret stays hidden. You will be refunded on Liquid.');
    const call = { from: wallet.address, to: NETWORK.escrow, data: W.build_claim_call(s.lock_id, W.revealed_value(s.secrets.lock_secret)) };
    await this.polygon.simulate(call);
    this.set(s, { status: `Waiting for ${wallet.name} to send the claim…` });
    const hash = await wallet.send(call);
    this.set(s, { status: 'Claim sent, waiting for Polygon to finalize it.', claim_hash: hash, claim_sent_at: now() });
  }

  // ---- P to L ----

  async lockOnPolygon(s) {
    const wallet = this.getWallet();
    if (!wallet) throw new Error('Connect a Polygon wallet first.');
    if (s.state !== 'lock') return;
    if (!same(wallet.address, s.polygon_owner)) throw new Error(`Connect the wallet this swap was agreed for, ${s.polygon_owner}.`);
    // Nothing from an earlier try may show as what the wallet asks now.
    this.set(s, { status: '' });
    if (await this.lock(s, 'latest')) return;
    if (await this.esplora[0].tip() > s.terms.h_accept + 15) throw new Error('The time to lock has passed, so the maker would no longer fill this. Nothing was sent.');
    const amount = BigInt(s.terms.polygon_amount);
    if (await this.polygon.tokenBalance(NETWORK.token, wallet.address) < amount) throw new Error(`This wallet holds too little ${NETWORK.tokenName}.`);
    if (await this.polygon.balance(wallet.address) < NETWORK.minPolWei) throw new Error('This wallet needs a little POL for the network fee.');
    // Each prompt says what it does, because a wallet shows only a contract call it can't explain.
    const sum = `${showUnits(amount)} ${NETWORK.tokenName}`;
    const approving = await this.polygon.allowance(NETWORK.token, wallet.address, NETWORK.escrow) < amount;
    if (approving) {
      const detail = `It lets the escrow take ${sum} from this wallet when you lock. Nothing moves yet.`;
      this.set(s, { status: `Step 1 of 2, in ${wallet.name}: approve. ${detail}` });
      const approve = { from: wallet.address, to: NETWORK.token, data: W.build_approve_call(NETWORK.escrow, amount.toString()) };
      const sent = await wallet.send(approve, { title: 'Step 1 of 2: approve', detail });
      this.set(s, { status: 'Step 1 of 2: approved. Waiting for Polygon to include it, a few seconds…' });
      await this.mined(sent, 'approval');
    }
    // Long enough on both clocks for the maker's check, and the time it takes to lock and be seen.
    const head = await this.polygon.block('latest');
    const secs = Number(s.terms.polygon_window_secs) + NETWORK.p2lLockMarginSecs;
    const blocks = Math.ceil((secs * 1000) / (await this.blockMs()));
    const data = W.build_lock_call(s.secrets.lock_address, s.terms.maker_polygon, amount.toString(), BigInt(head.time + secs), BigInt(head.number + blocks));
    const call = { from: wallet.address, to: NETWORK.escrow, data };
    await this.polygon.simulate(call);
    const detail = `It moves ${sum} into the escrow for this swap. The maker gets it only after locking your side; otherwise it comes back to you.`;
    this.set(s, { status: `${approving ? 'Step 2 of 2, in' : 'In'} ${wallet.name}: lock. ${detail}` });
    const hash = await wallet.send(call, { title: approving ? 'Step 2 of 2: lock' : 'Lock', detail });
    this.set(s, { state: 'await_maker', status: '', lock_hash: hash });
  }

  async tickP2L(s, tip) {
    const block = await this.polygon.block('finalized');
    const lock = await this.lock(s);
    s.tip = tip;
    if (lock?.state === 'refunded' && !s.claim_tx) s.state = 'refunded';
    if (lock?.state === 'claimed' && !s.claim_tx) {
      s.state = 'done';
      s.status = FINISHED_EARLIER;
    }
    if (s.state === 'release_asked' && now() - s.early_asked_at > EARLY_REFUND_WAIT_SECS) {
      s.state = 'await_maker';
      s.status = 'The maker didn’t answer. Nothing has changed, and the timed refund still protects you.';
    }
    if (s.claim_tx) await this.tickClaimed(s);
    else if (['await_maker', 'signing'].includes(s.state)) {
      const pastClaim = tip >= s.terms.lock_height - NETWORK.claimCutoffBlocks;
      const coins = await this.exactCoins(this.esplora[0], s);
      // Once the maker has locked it won't release, so the page stops offering to ask.
      s.maker_locked = coins.length > 0;
      s.maker_confirmations = coins.length ? Math.max(...coins.map((c) => c.confirmations)) : 0;
      const deep = coins.find((c) => c.confirmations >= confirmationsFor(s.terms));
      if (pastClaim || (lock && !claimable(lock, block))) {
        s.state = 'refund_wait';
      } else if (deep) {
        s.coin = { txid: deep.txid, vout: deep.vout, confirmations: deep.confirmations };
        await this.runSession(s, deep);
      }
    }
    if (s.state === 'refund_wait' && lock?.state === 'open' && !claimable(lock, block)) s.state = 'refund_ready';
    if (lock) s.lock = lock;
    this.save(s);
  }

  // The maker's coin must look the same from a second source before anything is signed, and the fee coin must be there.
  async runSession(s, coin) {
    if (s.signed && s.maker_share) return this.finishClaim(s);
    const second = this.esplora[1];
    if (second) {
      const other = await this.exactCoins(second, s).catch(() => []);
      if (!other.some((c) => c.txid === coin.txid && c.vout === coin.vout && c.confirmations >= confirmationsFor(s.terms))) {
        s.status = 'Waiting for the second Liquid source to see the maker’s coin.';
        return undefined;
      }
    }
    const fees = await this.esplora[0].readCoins(s.info.plain_fee_address, s.info.fee_script, s.secrets.blinding_key);
    const fee = fees.find((c) => c.is_lbtc && c.confirmations >= 1);
    if (!fee) {
      s.status = 'Waiting for the fee coin the maker sends.';
      return undefined;
    }
    s.state = 'signing';
    const open = this.sessions.get(s.id);
    // Already in the air: give it time. The maker keeps one session per swap, and a retry starts a fresh nonce.
    if (open && Date.now() - open.at < 30000) return undefined;
    if (s.signed) {
      this.send(s, { type: 'taker-share', body: { lock_point: s.secrets.lock_point, share: s.signed.share } });
      this.sessions.set(s.id, { at: Date.now() });
      return undefined;
    }
    const session = new W.ClaimSession(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point,
      coin_tx: coin.hex, coin_vout: coin.vout, fee_tx: await this.esplora[0].txHex(fee.txid), fee_vout: fee.vout,
      to_address: s.to_address, genesis: NETWORK.genesis,
    }));
    this.sessions.set(s.id, { at: Date.now(), session });
    this.send(s, { type: 'session-open', body: { lock_point: s.secrets.lock_point, tx: session.tx(), nonce: session.nonce() } });
    s.status = 'Signing the claim together with the maker…';
    return undefined;
  }

  onSessionNonce(s, nonce) {
    const open = this.sessions.get(s.id);
    if (!open?.session) return;
    const share = open.session.sign(nonce);
    // Saved before our share goes out: the maker answers once, and only this state can use its answer.
    s.signed = JSON.parse(open.session.signed_state());
    this.save(s);
    open.session.free();
    open.session = null;
    open.at = Date.now();
    this.send(s, { type: 'taker-share', body: { lock_point: s.secrets.lock_point, share } });
  }

  async onMakerShare(s, share) {
    if (!s.signed || s.claim_tx) return;
    s.maker_share = share;
    this.save(s);
    await this.finishClaim(s);
  }

  async finishClaim(s) {
    const tip = await this.esplora[0].tip();
    if (tip >= s.terms.lock_height - NETWORK.claimCutoffBlocks) {
      this.set(s, { state: 'refund_wait', status: 'Too close to the maker’s refund to reveal the secret, so it stays hidden.' });
      return;
    }
    const lock = await this.lock(s);
    if (!lock || lock.state !== 'open') throw new Error('Our Polygon lock is no longer open, so the claim was not published.');
    const hex = W.finish_claim(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, lock_point: s.secrets.lock_point, lock_secret: s.secrets.lock_secret,
      signed: s.signed, parents: await this.esplora[0].parentsOf(s.signed.tx), maker_share: s.maker_share, genesis: NETWORK.genesis,
    }));
    this.set(s, { claim_tx: hex, state: 'claiming', status: 'Publishing the claim…' });
    await this.tickClaimed(s);
  }

  // Published means the maker can read the secret, so keep sending until it confirms, or the maker could take both.
  async tickClaimed(s) {
    // From the transaction, not the reply: a lost reply would otherwise leave this waiting on a claim that has already confirmed.
    s.claim_txid ??= W.tx_id(s.claim_tx);
    const status = await this.esplora[0].status(s.claim_txid).catch(() => null);
    if (status?.confirmed) {
      s.state = 'done';
      s.status = 'You were paid on Liquid.';
      return;
    }
    try {
      await this.esplora[0].broadcast(s.claim_tx);
      s.status = 'Claim published, waiting for a confirmation.';
    } catch (e) {
      if (!ALREADY_SENT.test(e.message)) s.status = `Still trying to publish the claim: ${e.message.slice(0, 120)}`;
    }
  }

  // After both of the lock's deadlines. Any wallet can send this, and the escrow pays only the lock's initiator.
  async refundOnPolygon(s) {
    const wallet = this.getWallet();
    if (!wallet) throw new Error('Connect a Polygon wallet to pay the network fee. It can be any wallet: the refund only pays the swap’s owner.');
    if (s.claim_tx) throw new Error('The claim was published, so this swap can’t be refunded.');
    const call = { from: wallet.address, to: NETWORK.escrow, data: W.build_refund_call(s.lock_id) };
    await this.polygon.simulate(call);
    const hash = await wallet.send(call);
    this.set(s, { state: 'refunding', status: 'Refund sent.', refund_hash: hash });
  }

  // ---- L to P refund ----

  async tickRefund(s) {
    if (s.refund_txid) {
      const status = await this.esplora[0].status(s.refund_txid).catch(() => null);
      if (status?.confirmed) s.state = 'refunded';
      else if (s.refund_tx) await this.esplora[0].broadcast(s.refund_tx).catch(() => {});
    }
  }

  // Any coin at the address can come back, whatever it holds. It needs a small blinded L-BTC coin at the fee address.
  async refundOnLiquid(s, toAddress) {
    const coin = await this.refundCoin(s);
    if (!coin) throw new Error('There is no coin at the swap address to refund.');
    const fees = await this.esplora[0].readCoins(s.info.plain_fee_address, s.info.fee_script, s.secrets.blinding_key);
    const fee = fees.find((c) => c.is_lbtc && c.confirmations >= 1 && BigInt(c.amount) >= BigInt(NETWORK.refundFeeCoinSats));
    if (!fee) throw new Error(`Send at least ${NETWORK.refundFeeCoinSats} sats of L-BTC to the fee address first, and wait for a confirmation.`);
    const hex = W.build_refund(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, coin_tx: coin.hex, coin_vout: coin.vout,
      fee_tx: await this.esplora[0].txHex(fee.txid), fee_vout: fee.vout, to_address: toAddress, genesis: NETWORK.genesis,
    }));
    const txid = W.tx_id(hex);
    // Sent earlier with the reply lost: the transaction is out there, so carry on and watch it.
    await this.esplora[0].broadcast(hex).catch((e) => { if (!ALREADY_SENT.test(e.message)) throw e; });
    this.set(s, { refund_tx: hex, refund_txid: txid, state: 'refunding', status: 'Refund sent, waiting for a confirmation.' });
  }

  // ---- An early refund, if the maker agrees ----

  // The taker cancelled before paying or locking: tell the maker, so it frees what it held for this swap now rather than at the end
  // of its lock window. Best effort: the swap is cancelled here either way, and a maker that can't (an older one, on S→L) just waits.
  tellCancelled(s) {
    if (!s.maker || !s.secrets?.lock_point) return;
    try {
      this.send(s, { type: 'cancel-ask', body: { lock_point: s.secrets.lock_point } });
    } catch (e) {
      console.warn('telling the maker about a cancel', e?.message ?? e);
    }
  }

  // First the maker promises never to lock. The fee coin and the address can come afterwards, without a race.
  async askCancel(s) {
    // Paid, or holding only a coin of the wrong asset or amount, which the maker never locks for.
    const paid = s.state === 'await_maker' && s.coin;
    const wrong = s.state === 'fund' && s.wrong_coin;
    if (s.direction !== 'L2P' || !(paid || wrong)) return;
    this.send(s, { type: 'cancel-ask', body: { lock_point: s.secrets.lock_point } });
    this.set(s, { state: 'early_asked', early_asked_at: now(), status: '' });
  }

  // Only ever a request. Whatever the maker does, the timed refund is still there.
  async askEarlyRefund(s, toAddress) {
    if (s.state !== 'early_form') return;
    const coin = await this.refundCoin(s);
    if (!coin) throw new Error('There is no coin at the swap address to refund.');
    const fees = await this.esplora[0].readCoins(s.info.plain_fee_address, s.info.fee_script, s.secrets.blinding_key);
    const fee = fees.find((c) => c.is_lbtc && c.confirmations >= 1 && BigInt(c.amount) >= BigInt(NETWORK.refundFeeCoinSats));
    if (!fee) throw new Error(`Send at least ${NETWORK.refundFeeCoinSats} sats of L-BTC to the fee address first, and wait for a confirmation.`);
    const session = new W.EarlyRefundSession(JSON.stringify({
      swap: swapArgs(s), taker_sk: s.secrets.taker_sk, coin_tx: coin.hex, coin_vout: coin.vout,
      fee_tx: await this.esplora[0].txHex(fee.txid), fee_vout: fee.vout, to_address: toAddress, genesis: NETWORK.genesis,
    }));
    this.earlyRefunds.set(s.id, session);
    this.send(s, { type: 'early-refund-ask', body: { lock_point: s.secrets.lock_point, tx: session.tx, nonce: session.nonce } });
    this.set(s, { state: 'early_signing', early_asked_at: now(), status: '' });
  }

  async onEarlyRefundShare(s, body) {
    const session = this.earlyRefunds.get(s.id);
    if (s.state !== 'early_signing' || !session) return;
    this.earlyRefunds.delete(s.id);
    let hex;
    try {
      hex = session.finish(body.nonce, body.share);
    } catch (e) {
      this.set(s, { state: 'early_form', status: `The maker's answer didn't check out, so nothing was sent: ${e.message ?? e}` });
      return;
    }
    // Saved first, so the refund is watched and sent again whatever happens to this send.
    this.set(s, { refund_tx: hex, refund_txid: W.tx_id(hex), state: 'refunding', status: 'Refund sent, waiting for a confirmation.' });
    await this.esplora[0].broadcast(hex).catch(() => {});
  }

  // P→L, before the maker locks on Liquid. The escrow sends the coins back to the lock's own initiator, so the maker can only
  // say yes or no. Nothing secret goes out, and the timed refund stays either way.
  async askRelease(s) {
    if (s.direction !== 'P2L' || s.state !== 'await_maker' || s.maker_locked) return;
    this.send(s, { type: 'release-ask', body: { lock_point: s.secrets.lock_point } });
    this.set(s, { state: 'release_asked', early_asked_at: now(), status: '' });
  }

  // ---- Messages and ticks ----

  async onMessage(s, got) {
    if (got.sender !== s.maker || got.id !== s.id) return;
    const { type, body } = got.body;
    if (body.lock_point && body.lock_point !== s.secrets.lock_point) return;
    if (type === 'claim-offer' && s.direction === 'L2P') {
      this.pendingOffers.set(s.id, body);
      await this.answerPendingOffer(s);
    } else if (type === 'session-nonce' && s.direction === 'P2L') {
      this.onSessionNonce(s, body.nonce);
    } else if (type === 'early-refund-share' && s.direction === 'L2P') {
      await this.onEarlyRefundShare(s, body);
    } else if (type === 'cancelled' && s.direction === 'L2P' && s.state === 'early_asked') {
      this.set(s, { state: 'early_form', maker_cancelled: true, status: '' });
    } else if (type === 'rfq-reject' && s.state === 'early_signing') {
      this.earlyRefunds.delete(s.id);
      this.set(s, { state: 'early_form', status: 'The maker didn’t sign the refund. Check the fee coin and ask again, or wait for the timed refund.' });
    } else if (type === 'rfq-reject' && (s.state === 'early_asked' || s.state === 'release_asked')) {
      this.earlyRefunds.delete(s.id);
      this.set(s, { state: this.waiting(s), status: 'The maker can’t refund early now. Nothing has changed, and the timed refund still protects you.' });
    } else if (type === 'maker-share' && s.direction === 'P2L') {
      await this.onMakerShare(s, body.share);
    }
  }

  async tick(s) {
    // Older pages let a swap with a wrong coin expire. Look once per visit, so such a coin can still come back.
    if (s.state === 'expired' && s.direction === 'L2P' && !this.recheckedExpired.has(s.id)) {
      this.recheckedExpired.add(s.id);
      if ((await this.esplora[0].utxos(s.info.plain_address)).length) this.set(s, { state: 'refund', status: '' });
      return;
    }
    if (FINISHED.includes(s.state) || s.state === 'recovery') return;
    const tip = await this.esplora[0].tip();
    if (s.state === 'restored') await this.tickRestored(s, tip);
    else if (s.direction === 'L2P') await this.tickL2P(s, tip);
    else await this.tickP2L(s, tip);
  }
}
