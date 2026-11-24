//! The maker's main loop. From liq2sol's (docs/copied.md), with Polygon in Solana's place: docs/design.md section 4.
//!
//! Each tick: read both chains into a `View`, ask the engine, durably record the new state and the intended actions, then
//! carry them out through the signer. It holds no keys. After a crash it simply starts ticking again, because the chains and
//! the store say everything that is needed, and every action is safe to repeat.

#![forbid(unsafe_code)]

pub mod balances;
pub mod pager;
pub mod rfq;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2any_chain::esplora::{Esplora, Rpc};
use liq2any_chain::evm::{from_hex, Address as EvmAddress};
use liq2any_chain::liquid::{exact_secret_coin, is_key_path_spend, published_signature, usdt, use_network, NetworkConfig};
use liq2any_chain::polygon::{self, lock_id, Lock, Rpc as PolygonRpc, State as LockState};
use liq2any_maker_core::engine::{self, Action, Direction, EscrowState, EscrowView, HtlcView, Page, State, Terms, View};
use liq2any_maker_core::store::Store;
use liq2any_maker_core::LockPoint;
use liq2any_rfq::messages::{Body, ClaimOffer};
use liq2any_signer::{unhex, Call, Client, LiquidSource, Reply, SwapTerms};
use liq2any_swapkey::polygon_address_of;
use serde::{Deserialize, Serialize};
use simplicityhl::elements::encode::deserialize;
use simplicityhl::elements::{AddressParams, AssetId, OutPoint, Transaction as LiquidTx, Txid};

/// Polygon endpoints. The first is read for every decision; a second, if set, is read too, and new locks wait while they
/// disagree. Every transaction goes out through both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolygonSources {
    pub rpc: String,
    #[serde(default)]
    pub second_rpc: Option<String>,
    /// On a test network, the escrow and token. On mainnet they are pinned in the code.
    #[serde(default)]
    pub escrow: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// "testnet" (the default) or "mainnet". Both chains and the quotes' network must agree with it.
    #[serde(default = "liq2any_signer::default_network")]
    pub network: String,
    pub db: PathBuf,
    pub signer_socket: PathBuf,
    /// Pages are appended here. Something outside the daemon must watch this file.
    pub pages: PathBuf,
    pub engine: engine::Config,
    pub liquid: LiquidSource,
    /// An independent Liquid source. A lock isn't acted on unless both agree.
    pub second_liquid: Option<LiquidSource>,
    pub polygon: PolygonSources,
    pub tick_millis: u64,
    /// Don't repeat the same action for one swap sooner than this.
    pub retry_secs: u64,
    pub regtest_assets: Option<(String, String)>,
    /// Quotes over Nostr. Without this, swaps only enter through `accept`.
    pub rfq: Option<rfq::RfqConfig>,
    /// Pages also go to the operator as Nostr private messages. Without this, only the page file has them.
    #[serde(default)]
    pub page_to: Option<pager::PageTo>,
    #[serde(default)]
    pub balance_alarms: balances::BalanceAlarms,
}

/// The full terms of one swap, and how to reach its taker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Details {
    pub terms: SwapTerms,
    /// The taker's Nostr key. Only this key can take part in the swap's signing session.
    pub taker_nostr: String,
    /// The negotiation id, repeated on every message of the swap.
    pub negotiation: String,
}

/// Register a swap for the loop to pick up, refusing a point ever seen before.
pub fn accept(store: &Store, terms: &Terms, details: &Details) -> Result<bool, String> {
    let added = store.accept(terms, &State::WatchLock).map_err(|e| e.to_string())?;
    if added {
        let body = serde_json::to_string(details).map_err(|e| e.to_string())?;
        store.set_details(&terms.swap_id(), &body).map_err(|e| e.to_string())?;
    }
    Ok(added)
}

/// Where a top-up's cost is filed: under its transaction, since it belongs to no swap.
fn top_up_id(reply: &Reply) -> Result<[u8; 32], String> {
    let Reply::Liquid(text) = reply else {
        return Err(format!("signer: {reply:?}"));
    };
    let tx: LiquidTx = deserialize(&unhex(text)?).map_err(|e| e.to_string())?;
    unhex(&tx.txid().to_string())?.try_into().map_err(|_| "a txid of the wrong length".to_string())
}

/// The Polygon form of a lock point: the address the escrow stores.
pub fn lock_address(point: &LockPoint) -> Result<EvmAddress, String> {
    Ok(EvmAddress(polygon_address_of(&point.0).map_err(|e| e.to_string())?))
}

/// How many blocks back the block time is measured over, as the signer does.
const BLOCK_TIME_SPAN: u64 = 2_000;
/// Finalized heights of the two sources further apart than this count as disagreeing.
const POLYGON_SOURCES_APART: u64 = 50;

/// One tick's reading of Polygon, shared by every swap.
#[derive(Debug, Clone, Copy)]
struct PolygonTick {
    time: u64,
    block: u64,
    age_secs: u64,
    lag_blocks: u64,
    block_ms: u64,
    disagree: bool,
}

pub struct Maker {
    cfg: Config,
    store: Store,
    chain: Esplora,
    second: Option<Esplora>,
    polygon: PolygonRpc,
    second_polygon: Option<PolygonRpc>,
    ids: polygon::Ids,
    signer: Client,
    me: EvmAddress,
    fee_coin_sats: u64,
    last_finalized: (u64, Instant),
    last_sent: HashMap<([u8; 32], String), Instant>,
    last_paged: HashMap<([u8; 32], String), Instant>,
    last_top_up: Option<Instant>,
    /// The way to takers. Without it no L→P swap can get its adapter signature, so none will lock.
    pub outbox: Option<rfq::Outbox>,
    /// Where pages go out as private messages, when `page_to` is set.
    pub pager: Option<std::sync::mpsc::Sender<String>>,
    last_balance_check: Option<Instant>,
    /// Each low-balance alarm that holds, and when it last paged.
    low: HashMap<&'static str, Instant>,
}

/// How often to look at the fee coin pool. It is several chain reads, and a pool lasts many swaps.
const TOP_UP_EVERY: Duration = Duration::from_secs(60);

fn source(s: &LiquidSource) -> Esplora {
    match s {
        LiquidSource::Esplora(url) => Esplora::Http(url.clone()),
        LiquidSource::Node { cli, args } => Esplora::Node(Rpc { cli: cli.clone(), args: args.clone() }),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn action_name(a: &Action) -> String {
    format!("{a:?}").split([' ', '(']).next().unwrap_or("").to_string()
}

/// The escrow and token on a test network, from the config. Mainnet's are pinned in the code.
fn test_ids(p: &PolygonSources) -> Result<Option<(EvmAddress, EvmAddress)>, String> {
    let parse = |what: &str, s: &Option<String>| -> Result<Option<EvmAddress>, String> {
        s.as_deref().map(|a| EvmAddress::parse(a).map_err(|e| format!("refusing to start: polygon.{what}: {e}"))).transpose()
    };
    match (parse("escrow", &p.escrow)?, parse("token", &p.token)?) {
        (Some(escrow), Some(token)) => Ok(Some((escrow, token))),
        (None, None) => Ok(None),
        _ => Err("refusing to start: set both polygon.escrow and polygon.token, or neither".into()),
    }
}

/// Our view of a lock, for the engine: only an existing one, compared with what the terms say it should be.
fn escrow_view(lock: &Lock, lock_address: &EvmAddress, recipient: &EvmAddress) -> Option<EscrowView> {
    let state = match lock.state {
        LockState::None => return None,
        LockState::Open => EscrowState::Open,
        LockState::Claimed => EscrowState::Claimed,
        LockState::Refunded => EscrowState::Refunded,
    };
    Some(EscrowView {
        state,
        // A lock over 64 bits can't be ours, so reading it as zero only makes it fail every check.
        amount: u64::try_from(lock.amount).unwrap_or(0),
        lock_address_ok: lock.lock_address == *lock_address,
        recipient_ok: lock.recipient == *recipient,
        deadline_time: lock.deadline_time,
        deadline_block: lock.deadline_block,
        long_leg: lock.long_leg,
        extended: lock.extended,
        revealed: lock.revealed,
    })
}

impl Maker {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let chain = source(&cfg.liquid);
        let second = cfg.second_liquid.as_ref().map(source);
        let polygon_rpc = PolygonRpc::new(&cfg.polygon.rpc);
        let second_polygon = cfg.polygon.second_rpc.as_deref().map(PolygonRpc::new);
        let mut caps = vec![cfg.engine.max_per_swap];
        if let Some(rfq) = &cfg.rfq {
            caps.extend(rfq.pairs().iter().map(|p| p.max));
            // Test networks may name themselves, as regtest does. Mainnet's name is exact, and nothing else may use it.
            if (rfq.network == "mainnet") != (cfg.network == "mainnet") {
                return Err(format!("refusing to start: quotes are for {:?}, and the network is {:?}", rfq.network, cfg.network));
            }
        }
        let chain_id = polygon_rpc.chain_id()?;
        if let Some(second) = &second_polygon {
            if second.chain_id()? != chain_id {
                return Err("refusing to start: the two Polygon endpoints are on different chains".into());
            }
        }
        polygon::enter(&cfg.network, cfg.regtest_assets.is_some(), &caps, &chain.genesis()?, chain_id, test_ids(&cfg.polygon)?)?;
        let ids = polygon::ids();
        if let Some((usdt_id, lbtc_id)) = &cfg.regtest_assets {
            use_network(NetworkConfig {
                params: &AddressParams::ELEMENTS,
                usdt: AssetId::from_str(usdt_id).map_err(|e| e.to_string())?,
                lbtc: AssetId::from_str(lbtc_id).map_err(|e| e.to_string())?,
            });
        }
        let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
        let mut signer = Client::connect(&cfg.signer_socket)?;
        let Reply::Identity { polygon: me, fee_coin_sats, .. } = signer.call(&Call::Identity)? else {
            return Err("unexpected reply from the signer".into());
        };
        let me = EvmAddress::parse(&me)?;
        // Whatever was in flight at a crash is settled by reading the chains, so old intents need nothing more.
        for (id, hash, action) in store.unfinished_intents().map_err(|e| e.to_string())? {
            println!("resuming: {} may have been in flight for {}", action_name(&action), hex(&hash));
            store.finish_intent(id).map_err(|e| e.to_string())?;
        }
        let finalized = polygon_rpc.block("finalized")?.number;
        Ok(Self {
            cfg,
            store,
            chain,
            second,
            polygon: polygon_rpc,
            second_polygon,
            ids,
            signer,
            me,
            fee_coin_sats,
            last_finalized: (finalized, Instant::now()),
            last_sent: HashMap::new(),
            last_paged: HashMap::new(),
            last_top_up: None,
            outbox: None,
            pager: None,
            last_balance_check: None,
            low: HashMap::new(),
        })
    }

    fn page(&mut self, hash: &[u8; 32], page: &Page) {
        let key = (*hash, format!("{page:?}"));
        // Hold back repeats for ten minutes, but a new kind of page always goes out.
        if self.last_paged.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        self.last_paged.insert(key, Instant::now());
        let line = format!("{} PAGE {} {page:?}\n", now_secs(), hex(hash));
        eprint!("{line}");
        if let Some(pager) = &self.pager {
            let what = match page {
                Page::LowBalance(what) => what.clone(),
                other => format!("{other:?}"),
            };
            let on = if *hash == [0; 32] { String::new() } else { format!(", on swap {}", hex(hash)) };
            let _ = pager.send(format!("liq2any maker: {what}{on}"));
        }
        use std::io::Write;
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.cfg.pages)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("could not write the page file: {e}");
        }
    }

    /// Latch an alarm if Liquid's tip went down, or the block we last saw at a height has changed.
    fn check_rollback(&mut self, tip: u32) -> Result<bool, String> {
        let map = |e: liq2any_maker_core::store::StoreError| e.to_string();
        if self.store.rollback().map_err(map)? {
            return Ok(true);
        }
        if let Some((height, hash)) = self.store.last_tip().map_err(map)? {
            let moved = tip < height || self.chain.block_hash(height)? != hash;
            if moved {
                self.store.set_rollback(true).map_err(map)?;
                self.page(&[0; 32], &Page::Rollback);
                return Ok(true);
            }
        }
        let hash = self.chain.block_hash(tip)?;
        self.store.set_last_tip(tip, &hash).map_err(map)?;
        Ok(false)
    }

    fn maker_key(&mut self, swap_id: [u8; 32]) -> Result<LockPoint, String> {
        let Reply::SwapKey { key } = self.signer.call(&Call::SwapKey { swap_id })? else {
            return Err("unexpected reply from the signer".into());
        };
        Ok(key)
    }

    /// Keep fee coins ready, so a swap never waits for one. Failing here only delays a swap, and only until the next try.
    fn top_up_fee_coins(&mut self) {
        if self.last_top_up.is_some_and(|t| t.elapsed() < TOP_UP_EVERY) {
            return;
        }
        self.last_top_up = Some(Instant::now());
        match self.signer.call(&Call::TopUpFeeCoins) {
            Ok(Reply::Nothing) => {}
            Ok(reply) => {
                let sent = top_up_id(&reply).and_then(|id| self.send_liquid(&id, "TopUp", reply));
                if let Err(e) = sent {
                    eprintln!("could not top up the fee coins: {}", e.chars().take(200).collect::<String>());
                }
            }
            Err(e) => eprintln!("could not top up the fee coins: {e}"),
        }
    }

    /// Everything open except this swap, in this direction: what a new lock would add to.
    fn open_exposure(&self, terms: &Terms) -> Result<u64, String> {
        let open = self.store.open_swaps().map_err(|e| e.to_string())?;
        Ok(open
            .iter()
            .filter(|(t, s)| t.direction == terms.direction && t.lock_point != terms.lock_point && *s != State::WatchLock)
            .map(|(t, _)| t.polygon_amount)
            .sum())
    }

    fn coin_view(&mut self, terms: &Terms, swap: &SwapTerms, tip: u32) -> Result<(HtlcView, bool), String> {
        let id = terms.swap_id();
        let key = self.maker_key(id)?;
        let address = swap.address(&key)?;
        let blinding = swap.blinding()?;
        let map = |e: liq2any_maker_core::store::StoreError| e.to_string();
        let exact = exact_secret_coin(&self.chain.secret_coins(&address, &blinding)?, usdt(), swap.liquid_amount, tip, 0);
        if let Some(found) = exact {
            let outpoint = found.coin.outpoint;
            self.store.remember_coin(&id, &outpoint.txid.to_string(), outpoint.vout).map_err(map)?;
            let confirmations = found.coin.height.map_or(0, |h| (tip + 1).saturating_sub(h));
            let mut disagree = false;
            if let Some(second) = &self.second {
                // A second source we can't read counts as disagreeing. That stops new locks, and must never stop a claim.
                disagree = match (second.secret_coins(&address, &blinding), second.tip()) {
                    (Ok(coins), Ok(tip)) => exact_secret_coin(&coins, usdt(), swap.liquid_amount, tip, 0).is_none(),
                    _ => true,
                };
            }
            return Ok((HtlcView::Unspent { confirmations }, disagree));
        }
        // Our own lock's coin is known even before it confirms, in case it is spent before we ever see it unspent. The swap is
        // always the lock's first output.
        if terms.direction == Direction::P2L {
            if let Some(raw) = self.store.stored_lock_tx(&id).map_err(map)? {
                let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
                self.store.remember_coin(&id, &tx.txid().to_string(), 0).map_err(map)?;
            }
        }
        let Some((txid, vout)) = self.store.seen_coin(&id).map_err(map)? else {
            return Ok((HtlcView::Absent, false));
        };
        let outpoint = OutPoint::new(Txid::from_str(&txid).map_err(|e| e.to_string())?, vout);
        let Some(spender) = self.chain.spender(&outpoint)? else {
            return Ok((HtlcView::Absent, false));
        };
        let confirmed = self.chain.is_confirmed(&spender.txid())?;
        if is_key_path_spend(&spender, &outpoint) != Some(true) {
            return Ok((HtlcView::SpentByRefund { confirmed }, false));
        }
        // Zero opens no point, so a signature we can't read pages a human instead of being trusted.
        let signature = published_signature(&spender, &outpoint).ok_or("a key-path spend with no signature")?;
        let call = Call::ExtractSecret { lock_point: terms.lock_point, signature: hex(&signature) };
        let secret = match self.signer.call(&call)? {
            Reply::Secret(secret) => secret,
            _ => [0; 32],
        };
        Ok((HtlcView::SpentByClaim { secret, confirmed }, false))
    }

    /// The swap's lock on Polygon, at `finalized`: ours for L→P, the taker's for P→L. With a second source, whether it agrees.
    fn lock_view(&self, swap: &SwapTerms) -> Result<(Option<EscrowView>, bool), String> {
        let taker = swap.taker()?;
        let (initiator, recipient) = match swap.direction {
            Direction::L2P => (self.me, taker),
            Direction::P2L => (taker, self.me),
        };
        let address = lock_address(&swap.lock_point)?;
        let id = lock_id(&initiator, &address);
        let lock = self.polygon.lock_at(&self.ids.escrow, &id, "finalized")?;
        // A second source we can't read, or that reads it otherwise, counts as disagreeing: new locks wait, claims don't.
        let disagree = match &self.second_polygon {
            Some(second) => second.lock_at(&self.ids.escrow, &id, "finalized").map_or(true, |other| {
                (other.state, other.amount, other.recipient, other.deadline_time, other.deadline_block)
                    != (lock.state, lock.amount, lock.recipient, lock.deadline_time, lock.deadline_block)
            }),
            None => false,
        };
        Ok((escrow_view(&lock, &address, &recipient), disagree))
    }

    /// Polygon's finalized block, how long since it moved, how far behind the head it is, and the measured block time.
    fn polygon_tick(&mut self) -> Result<PolygonTick, String> {
        let f = self.polygon.block("finalized")?;
        let head = self.polygon.block("latest")?.number;
        let past = self.polygon.block_number_at(f.number.saturating_sub(BLOCK_TIME_SPAN))?;
        let block_ms = f.time.saturating_sub(past.time).saturating_mul(1_000) / f.number.saturating_sub(past.number).max(1);
        if f.number != self.last_finalized.0 {
            self.last_finalized = (f.number, Instant::now());
        }
        let disagree = match &self.second_polygon {
            Some(second) => second.block("finalized").map_or(true, |b| b.number.abs_diff(f.number) > POLYGON_SOURCES_APART),
            None => false,
        };
        Ok(PolygonTick {
            time: f.time,
            block: f.number,
            age_secs: self.last_finalized.1.elapsed().as_secs(),
            lag_blocks: head.saturating_sub(f.number),
            block_ms,
            disagree,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn view(
        &mut self,
        terms: &Terms,
        swap: &SwapTerms,
        tip: u32,
        tip_age: u64,
        rollback: bool,
        poly: &PolygonTick,
    ) -> Result<View, String> {
        let (htlc, coin_disagrees) = self.coin_view(terms, swap, tip)?;
        let (escrow, lock_disagrees) = self.lock_view(swap)?;
        let adaptor_held = match self.signer.call(&Call::AdaptorHeld { swap_id: terms.swap_id() })? {
            Reply::Held(held) => held,
            reply => return Err(format!("signer: {reply:?}")),
        };
        let tips_disagree = match &self.second {
            Some(second) => second.tip().map_or(true, |t| t.abs_diff(tip) > 2),
            None => false,
        };
        let blocked = |who: &EvmAddress| self.polygon.is_blocked(&self.ids.token, who, "latest");
        Ok(View {
            liquid_tip: tip,
            liquid_tip_age_secs: tip_age,
            polygon_time: poly.time,
            polygon_block: poly.block,
            polygon_age_secs: poly.age_secs,
            polygon_lag_blocks: poly.lag_blocks,
            polygon_block_ms: poly.block_ms,
            rollback_detected: rollback,
            sources_disagree: coin_disagrees || tips_disagree || lock_disagrees || poly.disagree,
            maker_unblocked: !blocked(&self.me)?,
            taker_unblocked: !blocked(&swap.taker()?)?,
            adaptor_held,
            htlc,
            escrow,
            open_exposure: self.open_exposure(terms)?,
        })
    }

    /// Send a signed Polygon transaction through every endpoint. "Already known" means a retry, which is expected.
    fn send_polygon_raw(&self, raw: &[u8]) -> Result<(), String> {
        let first = self.polygon.send_raw(raw).map(|_| ());
        if let Some(second) = &self.second_polygon {
            let _ = second.send_raw(raw);
        }
        match first {
            Err(e) if e.contains("nonce too low") => Ok(()),
            other => other,
        }
    }

    fn send_polygon(&mut self, reply: Reply) -> Result<(), String> {
        let Reply::Polygon { raw, nonce, .. } = reply else {
            return Err(format!("signer: {reply:?}"));
        };
        self.send_polygon_raw(&from_hex(&raw)?).map_err(|e| format!("nonce {nonce}: {e}"))
    }

    /// Resend whatever the signer has signed and the chain hasn't mined, lowest nonce first, so a stuck one never holds up a
    /// claim behind it. The signer re-signs at a higher fee where the network's has passed it.
    fn send_pending_polygon(&mut self) {
        match self.signer.call(&Call::PendingPolygon) {
            Ok(Reply::PolygonPending(raws)) => {
                for raw in raws {
                    if let Err(e) = from_hex(&raw).and_then(|r| self.send_polygon_raw(&r)) {
                        eprintln!("could not resend a pending Polygon transaction: {}", e.chars().take(200).collect::<String>());
                    }
                }
            }
            other => eprintln!("could not list pending Polygon transactions: {other:?}"),
        }
    }

    /// Let the escrow take our USDT0, once. Refused by the signer when the call would fail.
    fn approve_escrow(&mut self) -> Result<(), String> {
        let reply = self.signer.call(&Call::ApproveEscrow)?;
        self.send_polygon(reply)
    }

    fn note_cost(&self, swap: &[u8; 32], kind: &str, lbtc_sats: u64, polygon_gwei: i64) {
        if let Err(e) = self.store.record_cost(swap, kind, lbtc_sats, polygon_gwei) {
            eprintln!("could not record a cost: {e}");
        }
    }

    fn send_liquid(&mut self, swap: &[u8; 32], kind: &str, reply: Reply) -> Result<Vec<u8>, String> {
        let Reply::Liquid(hex) = reply else {
            return Err(format!("signer: {reply:?}"));
        };
        let raw = unhex(&hex)?;
        let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
        match self.chain.broadcast(&tx) {
            Ok(_) => {}
            // Already known or mined means it worked before, which is the expected case for a retry.
            Err(e) if e.contains("already") || e.contains("txn-mempool-conflict") => {}
            Err(e) => return Err(e),
        }
        let fee: u64 = tx.output.iter().filter(|o| o.is_fee()).filter_map(|o| o.value.explicit()).sum();
        self.note_cost(swap, kind, fee, 0);
        Ok(raw)
    }

    fn offer_claim(&mut self, details: &Details) -> Result<(), String> {
        let lock_point = details.terms.lock_point.to_hex();
        match self.signer.call(&Call::PrepareClaim { terms: details.terms.clone() })? {
            Reply::Nothing => Ok(()),
            Reply::ClaimTemplate { tx, nonce } => {
                let outbox = self.outbox.as_ref().ok_or("no relay is configured, so the taker can't be reached")?;
                let body = Body::ClaimOffer(ClaimOffer { lock_point, tx, nonce });
                outbox.send(&details.taker_nostr, &details.negotiation, body)
            }
            // The pool ran out, so look at it again now, not in a minute.
            Reply::Failed(e) if e.contains("fee coin") => {
                self.last_top_up = None;
                Err(e)
            }
            reply => Err(format!("signer: {reply:?}")),
        }
    }

    fn execute(&mut self, terms: &Terms, details: &Details, action: &Action) -> Result<(), String> {
        let id = terms.swap_id();
        let lock_point = terms.lock_point;
        let t = &details.terms;
        let kind = action_name(action);
        match (action, t.direction) {
            (Action::RequestAdaptor, Direction::L2P) => self.offer_claim(details),
            (Action::LockPolygon { deadline_time, deadline_block, .. }, Direction::L2P) => {
                let call = Call::LockPolygon { terms: t.clone(), deadline_time: *deadline_time, deadline_block: *deadline_block };
                match self.signer.call(&call)? {
                    // Not approved yet: approve, and the engine asks again on a later tick, with the same deadlines.
                    Reply::Failed(e) if e.contains("ApproveEscrow") => {
                        self.approve_escrow()?;
                        Err("approved the escrow first; the lock follows".into())
                    }
                    reply => {
                        self.send_polygon(reply)?;
                        // The gas drop is ours to give, so it is a cost of the swap.
                        self.note_cost(&id, "GasDrop", 0, (t.gas_drop / 1_000_000_000) as i64);
                        Ok(())
                    }
                }
            }
            (Action::LockLiquid | Action::RebroadcastLock, Direction::P2L) => {
                // The signer returns the same bytes every time, so this can't pay twice. They are kept here too, so the
                // coin's outpoint is known at once.
                let reply = self.signer.call(&Call::LockLiquid { terms: t.clone() })?;
                let Reply::Liquid(ref text) = reply else {
                    return Err(format!("signer: {reply:?}"));
                };
                let raw = unhex(text)?;
                self.store.store_lock_tx(&id, &raw).map_err(|e| e.to_string())?;
                // A rebroadcast is the same transaction, so it shares the lock's name. The taker's fee coin goes with it.
                self.send_liquid(&id, "LockLiquid", reply)?;
                self.note_cost(&id, "FeeCoinForTaker", self.fee_coin_sats, 0);
                Ok(())
            }
            (Action::ClaimLiquid { secret }, Direction::L2P) => {
                let reply = self.signer.call(&Call::ClaimLiquid { terms: t.clone(), secret: *secret })?;
                self.send_liquid(&id, &kind, reply).map(|_| ())
            }
            (Action::ClaimPolygon { secret }, Direction::P2L) => {
                let call = Call::ClaimPolygon { taker_polygon: t.taker_polygon.clone(), lock_point, secret: *secret };
                let reply = self.signer.call(&call)?;
                self.send_polygon(reply)
            }
            (Action::RefundPolygon, Direction::L2P) => {
                let reply = self.signer.call(&Call::RefundPolygon { lock_point })?;
                self.send_polygon(reply)
            }
            (Action::RefundLiquid, Direction::P2L) => {
                let reply = self.signer.call(&Call::RefundLiquid { terms: t.clone() })?;
                self.send_liquid(&id, &kind, reply).map(|_| ())
            }
            (Action::Extend { new_time, new_block }, Direction::P2L) => {
                let call =
                    Call::Extend { taker_polygon: t.taker_polygon.clone(), lock_point, new_time: *new_time, new_block: *new_block };
                let reply = self.signer.call(&call)?;
                self.send_polygon(reply)
            }
            (Action::Release, Direction::P2L) => {
                let call = Call::Release { taker_polygon: t.taker_polygon.clone(), lock_point };
                let reply = self.signer.call(&call)?;
                self.send_polygon(reply)
            }
            (Action::Page(page), _) => {
                self.page(&id, page);
                Ok(())
            }
            (other, _) => Err(format!("{} does not fit this swap's direction", action_name(other))),
        }
    }

    fn tick_swap(
        &mut self,
        terms: &Terms,
        state: &State,
        tip: u32,
        tip_age: u64,
        rollback: bool,
        poly: &PolygonTick,
    ) -> Result<(), String> {
        let hash = terms.swap_id();
        let body = self.store.details(&hash).map_err(|e| e.to_string())?.ok_or("a swap with no details")?;
        let details: Details = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let view = self.view(terms, &details.terms, tip, tip_age, rollback, poly)?;
        let (next, actions) = engine::step(&self.cfg.engine, terms, state, &view);

        // Hold back repeats of an action already sent recently. A first send always goes out.
        let retry = Duration::from_secs(self.cfg.retry_secs);
        let due: Vec<Action> = actions
            .into_iter()
            .filter(|a| {
                matches!(a, Action::Page(_))
                    || self.last_sent.get(&(hash, action_name(a))).is_none_or(|t| t.elapsed() >= retry)
            })
            .collect();

        // Commit first, so a crash can't lose track of what may be in flight.
        let ids = self.store.commit_step(&hash, &next, &due).map_err(|e| e.to_string())?;
        // Released or refunded early meanwhile, from the quote thread: the store keeps it abandoned, and nothing goes out.
        if next != State::Abandoned && self.store.state(&hash).ok().flatten().is_some_and(|(_, s)| s == State::Abandoned) {
            println!("{} {:?}: abandoned meanwhile, so it stays abandoned and nothing is sent", hex(&hash), terms.direction);
            return Ok(());
        }
        if next != *state {
            println!("{} {:?}: {state:?} -> {next:?}", hex(&hash), terms.direction);
        }
        for (id, action) in ids.into_iter().zip(due) {
            self.last_sent.insert((hash, action_name(&action)), Instant::now());
            match self.execute(terms, &details, &action) {
                Ok(()) => println!("{} did {}", hex(&hash), action_name(&action)),
                // The engine asks again on a later tick, if it is still needed.
                Err(e) => eprintln!("{} {} failed: {}", hex(&hash), action_name(&action), e.chars().take(300).collect::<String>()),
            }
            self.store.finish_intent(id).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// One pass over every open swap. A failure on one swap doesn't hold up the others.
    pub fn tick(&mut self) -> Result<(), String> {
        let tip = self.chain.tip()?;
        let tip_age = now_secs().saturating_sub(self.chain.tip_time()?);
        let rollback = self.check_rollback(tip)?;
        if !rollback {
            self.top_up_fee_coins();
        }
        self.send_pending_polygon();
        let poly = self.polygon_tick()?;
        for (terms, state) in self.store.open_swaps().map_err(|e| e.to_string())? {
            if let Err(e) = self.tick_swap(&terms, &state, tip, tip_age, rollback, &poly) {
                eprintln!("{}: {}", hex(&terms.swap_id()), e.chars().take(300).collect::<String>());
            }
        }
        self.check_balances();
        Ok(())
    }

    /// Page when a wallet can't fill the largest trade, or pay for a few more swaps. Once when it starts, then now and then.
    /// Also approves the escrow when its allowance runs low, which happens once in practice.
    fn check_balances(&mut self) {
        let a = self.cfg.balance_alarms.clone();
        if self.last_balance_check.is_some_and(|t| t.elapsed() < Duration::from_secs(a.every_secs)) {
            return;
        }
        self.last_balance_check = Some(Instant::now());
        let holdings = match self.signer.call(&Call::Balances) {
            Ok(Reply::Balances { liquid_usdt, liquid_lbtc_sats, polygon_usdt, pol_wei, polygon_allowance, .. }) => {
                if polygon_allowance < u128::from(self.cfg.engine.max_per_swap) {
                    if let Err(e) = self.approve_escrow() {
                        eprintln!("could not approve the escrow: {e}");
                    }
                }
                balances::Holdings { liquid_usdt, liquid_lbtc_sats, polygon_usdt: u128::from(polygon_usdt), pol_wei }
            }
            other => {
                eprintln!("could not read our balances: {other:?}");
                return;
            }
        };
        let max_trade = self
            .cfg
            .rfq
            .as_ref()
            .map_or(u64::MAX, |r| r.pairs().iter().map(|p| p.max).max().unwrap_or(0))
            .min(self.cfg.engine.max_per_swap);
        let alarms = balances::low(&holdings, max_trade, &a);
        self.low.retain(|name, _| alarms.iter().any(|(n, _)| n == name));
        for (name, what) in alarms {
            if self.low.get(name).is_some_and(|t| t.elapsed() < Duration::from_secs(a.repeat_secs)) {
                continue;
            }
            self.low.insert(name, Instant::now());
            self.page(&[0; 32], &Page::LowBalance(what));
        }
    }
}

/// Paths in a config are relative to the config file, so a maker's folder can be copied without pointing at the original.
fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut cfg: Config = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let base = path.parent().unwrap_or(Path::new("."));
    // An absolute path is kept as it is, because joining onto one replaces what came before.
    cfg.db = base.join(&cfg.db);
    cfg.signer_socket = base.join(&cfg.signer_socket);
    cfg.pages = base.join(&cfg.pages);
    if let Some(rfq) = cfg.rfq.as_mut() {
        rfq.nostr_key = base.join(&rfq.nostr_key);
    }
    if let Some(page_to) = &cfg.page_to {
        page_to.check()?;
    }
    Ok(cfg)
}

/// Pages are sent by the maker's own Nostr key, so the operator's app shows which maker it is. Without one, a fresh key.
fn page_keys(cfg: &Config) -> Result<nostr::prelude::Keys, String> {
    match &cfg.rfq {
        Some(rfq) => rfq::load_keys(&rfq.nostr_key),
        None => Ok(nostr::prelude::Keys::generate()),
    }
}

/// Send one test page, and say what each relay answered.
pub fn test_page(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let page_to = cfg.page_to.as_ref().ok_or("no page_to in the config")?;
    let keys = page_keys(&cfg)?;
    println!("sending a test page from {}", keys.public_key().to_hex());
    let mut delivered = false;
    for (url, outcome) in pager::send(&keys, page_to, "liq2any maker: a test page. Pages will arrive like this.") {
        match outcome {
            Ok(()) => {
                delivered = true;
                println!("{url}: accepted");
            }
            Err(e) => println!("{url}: {e}"),
        }
    }
    if delivered {
        Ok(())
    } else {
        Err("no relay accepted the page".into())
    }
}

/// What this maker has earned and spent, from its own records. Read-only in effect, and safe while the maker runs.
pub fn report(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
    let r = store.report().map_err(|e| e.to_string())?;
    let money = |units: i128| format!("{:.6}", units as f64 / 1e6);
    println!("{:<12} {:<4} {:<10} {:>6} {:>14} {:>12}", "day (UTC)", "dir", "outcome", "swaps", "volume", "spread");
    let mut totals = (0u64, 0i128, 0i128);
    for ((day, direction, outcome), line) in &r.swaps {
        let day = day.as_deref().unwrap_or("earlier");
        println!("{day:<12} {direction:<4} {outcome:<10} {:>6} {:>14} {:>12}", line.swaps, money(line.volume), money(line.spread));
        totals = (totals.0 + line.swaps, totals.1 + line.volume, totals.2 + line.spread);
    }
    println!("{:<12} {:<4} {:<10} {:>6} {:>14} {:>12}", "total", "", "", totals.0, money(totals.1), money(totals.2));
    println!();
    // Polygon gas isn't counted yet: only gas drops, which are ours to give. Liquid fees are counted in full.
    println!("{:<12} {:>12} {:>16}", "day (UTC)", "L-BTC sats", "POL in drops");
    let mut spent = (0u64, 0i64);
    for (day, (sats, gwei)) in &r.costs {
        println!("{day:<12} {sats:>12} {:>16.9}", *gwei as f64 / 1e9);
        spent = (spent.0 + sats, spent.1 + gwei);
    }
    println!("{:<12} {:>12} {:>16.9}", "total", spent.0, spent.1 as f64 / 1e9);
    println!();
    println!("{} open, {} never started.", r.open, r.abandoned);
    if store.paused().map_err(|e| e.to_string())? {
        println!("Paused: no new swaps until `resume`.");
    }
    println!();
    // From the running signer, which holds the keys. The rest of the report needs only the store.
    let balances = Client::connect(&cfg.signer_socket).and_then(|mut c| c.call(&Call::Balances).map_err(|e| e.to_string()));
    match balances {
        Ok(Reply::Balances { liquid_usdt, liquid_lbtc_sats, free_fee_coins, polygon_usdt, pol_wei, .. }) => {
            println!(
                "Liquid  {:>14} USDt   {liquid_lbtc_sats:>12} L-BTC sats, {free_fee_coins} fee coins ready",
                format!("{:.8}", liquid_usdt as f64 / 1e8)
            );
            println!("Polygon {:>14} USDT0  {:>12.6} POL", money(polygon_usdt as i128), pol_wei as f64 / 1e18);
        }
        other => println!("Balances: can't ask the signer ({other:?})."),
    }
    Ok(())
}

/// `liq2any-maker withdraw <config> <liquid|polygon> <usdt|gas> <amount|all>`: the operator moves funds out, always to
/// `withdraw_to` in the signer's config. Refused while any swap is open: taking gas could leave the maker unable to claim, and
/// taking USDT could fail a lock it has promised.
pub fn withdraw(config_path: &Path, chain: &str, asset: &str, amount: &str) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let (chain, decimals_usdt, decimals_gas, gas_name) = match chain {
        "liquid" => (liq2any_signer::Chain::Liquid, 8, 8, "L-BTC"),
        "polygon" => (liq2any_signer::Chain::Polygon, 6, 18, "POL"),
        _ => return Err("the chain is liquid or polygon".into()),
    };
    let (asset, decimals, name) = match asset {
        "usdt" => (liq2any_signer::Asset::Usdt, decimals_usdt, "USDT"),
        "gas" => (liq2any_signer::Asset::Gas, decimals_gas, gas_name),
        _ => return Err("the asset is usdt or gas".into()),
    };
    let units = if amount.eq_ignore_ascii_case("all") { None } else { Some(parse_units(amount, decimals)?) };
    let open = Store::open(&cfg.db).map_err(|e| e.to_string())?.report().map_err(|e| e.to_string())?.open;
    if open > 0 {
        return Err(format!(
            "refusing: {open} swaps are still open, and they may need these funds. `pause` stops new ones, so try again once the report shows none open."
        ));
    }
    let mut signer = Client::connect(&cfg.signer_socket)?;
    let call = Call::Withdraw { chain, asset, amount: units };
    let what = units.map_or_else(|| format!("all the {name}"), |_| format!("{amount} {name}"));
    match signer.call(&call)? {
        Reply::Liquid(text) => println!("sent {what}: Liquid transaction {}", broadcast_liquid(&cfg, &text)?),
        Reply::EarlierWithdrawal(text) => println!(
            "an earlier Liquid withdrawal, {}, isn't confirmed yet, so it was sent again and nothing new was signed. Ask again once it confirms.",
            broadcast_liquid(&cfg, &text)?
        ),
        Reply::Polygon { raw, hash, .. } => {
            PolygonRpc::new(&cfg.polygon.rpc).send_raw(&from_hex(&raw)?)?;
            println!("sent {what}: Polygon transaction 0x{hash}");
        }
        Reply::Failed(e) => return Err(format!("the signer didn't withdraw: {e}")),
        other => return Err(format!("the signer didn't withdraw: {other:?}")),
    }
    Ok(())
}

fn broadcast_liquid(cfg: &Config, text: &str) -> Result<String, String> {
    let tx: LiquidTx = deserialize(&unhex(text)?).map_err(|e| e.to_string())?;
    match source(&cfg.liquid).broadcast(&tx) {
        Ok(_) => {}
        Err(e) if e.contains("already") || e.contains("txn-mempool-conflict") => {}
        Err(e) => return Err(e),
    }
    Ok(tx.txid().to_string())
}

/// A decimal amount in whole units, such as 12.5, in base units. More decimals than the asset has are refused, not rounded.
fn parse_units(text: &str, decimals: u32) -> Result<u128, String> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // A point must have digits after it: "1." is more likely a slip than 1.
    if !(digits(whole) || (whole.is_empty() && digits(frac))) || (text.contains('.') && !digits(frac)) {
        return Err(format!("{text} isn't an amount: use a number such as 12.5, or all"));
    }
    if frac.len() > decimals as usize {
        return Err(format!("{text} has more than {decimals} decimals"));
    }
    let scale = 10u128.pow(decimals);
    let whole: u128 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| format!("{text} is too large"))? };
    let frac: u128 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<width$}", width = decimals as usize).parse().map_err(|_| format!("{text} isn't an amount"))?
    };
    whole.checked_mul(scale).and_then(|w| w.checked_add(frac)).filter(|u| *u > 0).ok_or_else(|| format!("{text} is zero or too large"))
}

/// `liq2any-maker pause|resume <config>`: stop taking new swaps, or start again. Open swaps carry on either way, so a pause
/// lets them drain, such as before a withdrawal.
pub fn set_paused(config_path: &Path, on: bool) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
    store.set_paused(on).map_err(|e| e.to_string())?;
    let open = store.report().map_err(|e| e.to_string())?.open;
    if on {
        println!("paused: no new swaps. {open} still open, which carry on; withdraw once none are. `resume` starts taking swaps again.");
    } else {
        println!("taking new swaps again.");
    }
    Ok(())
}

/// For an operator who has checked the chain. New locks and all other actions resume on the next tick.
pub fn clear_rollback(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let store = Store::open(&cfg.db).map_err(|e| e.to_string())?;
    let was_set = store.rollback().map_err(|e| e.to_string())?;
    store.set_rollback(false).map_err(|e| e.to_string())?;
    println!("rollback alarm was {}, and is now clear", if was_set { "set" } else { "not set" });
    Ok(())
}

/// Run until stopped. A chain or signer outage just means trying again.
pub fn run(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    let tick = Duration::from_millis(cfg.tick_millis);
    let mut maker = loop {
        match Maker::new(cfg.clone()) {
            Ok(m) => break m,
            Err(e) if e.starts_with("refusing") => return Err(e),
            Err(e) => {
                eprintln!("waiting to start: {e}");
                // Not faster than this: if a public API is rate limiting us, retrying only prolongs it.
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    };
    if let Some(rfq_cfg) = cfg.rfq.clone() {
        let (pubkey, outbox) = rfq::start(rfq_cfg, &cfg)?;
        maker.outbox = Some(outbox);
        println!("maker answering quotes as {}", pubkey.to_hex());
    }
    if let Some(page_to) = cfg.page_to.clone() {
        maker.pager = Some(pager::start(page_keys(&cfg)?, page_to));
        println!("pages also go to {} as Nostr private messages", cfg.page_to.as_ref().map_or("", |p| p.nostr.as_str()));
    }
    println!("maker running");
    loop {
        if let Err(e) = maker.tick() {
            eprintln!("tick failed: {}", e.chars().take(300).collect::<String>());
            if e.contains("Broken pipe") || e.contains("EOF") {
                if let Ok(client) = Client::connect(&cfg.signer_socket) {
                    maker.signer = client;
                }
            }
        }
        std::thread::sleep(tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn withdrawal_amounts_are_read_exactly() {
        assert_eq!(parse_units("12.5", 6), Ok(12_500_000));
        assert_eq!(parse_units("0.00000001", 8), Ok(1));
        assert_eq!(parse_units(".5", 9), Ok(500_000_000));
        assert_eq!(parse_units("3", 8), Ok(300_000_000));
        assert_eq!(parse_units("250", 18), Ok(250_000_000_000_000_000_000), "POL amounts past 64 bits");
        assert!(parse_units("0.0000001", 6).is_err(), "more decimals than the asset has is refused, not rounded");
        for bad in ["", ".", "1.", "-1", "1e3", "1,5", "abc", "0", "0.000", "999999999999999999999999999999999999999999"] {
            assert!(parse_units(bad, 6).is_err(), "{bad:?} is refused");
        }
    }

    fn try_loaded(folder: &str, edit: impl Fn(&str) -> String) -> Result<Config, String> {
        let dir = std::env::temp_dir().join(format!("liq2any-config-{}-{folder}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("maker.json");
        std::fs::write(&path, edit(include_str!("../../../deploy/maker.example.json"))).unwrap();
        load_config(&path)
    }

    fn loaded(folder: &str, edit: impl Fn(&str) -> String) -> Config {
        try_loaded(folder, edit).unwrap()
    }

    #[test]
    fn page_to_is_optional_but_checked_when_set() {
        assert_eq!(loaded("no-pager", str::to_string).page_to, None);
        let with = |nostr: &str, relays: &str| {
            let page_to = format!("{{\"page_to\": {{\"nostr\": \"{nostr}\", \"relays\": {relays}}},");
            move |s: &str| s.replacen('{', &page_to, 1)
        };
        // NIP-19's own example key.
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        let cfg = loaded("pager", with(npub, "[\"wss://relay.example\"]"));
        assert_eq!(cfg.page_to.unwrap().check().unwrap().to_hex(), "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e");
        assert!(try_loaded("pager-bad-key", with("npub1notakey", "[\"wss://relay.example\"]")).is_err(), "a bad key stops the maker");
        assert!(try_loaded("pager-no-relay", with(npub, "[]")).is_err(), "and so does nowhere to send");
    }

    /// The mainnet example loads, offers within mainnet's cap per swap, pins nothing of a test network's, and its pairs pass the
    /// same check the maker runs at startup.
    #[test]
    fn the_mainnet_example_is_usable_as_it_is() {
        let dir = std::env::temp_dir().join(format!("liq2any-config-{}-mainnet", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("maker.json");
        std::fs::write(&path, include_str!("../../../deploy/maker.mainnet.example.json")).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.network, "mainnet");
        assert_eq!((cfg.polygon.escrow.as_deref(), cfg.polygon.token.as_deref()), (None, None), "mainnet's are pinned in the code");
        assert!(cfg.engine.max_per_swap <= liq2any_chain::liquid::MAINNET_MAX_PER_SWAP);
        let rfq = cfg.rfq.as_ref().unwrap();
        assert_eq!(rfq.network, "mainnet");
        assert_eq!(rfq::pairs_problem(&rfq.pairs, &cfg.engine), None);
    }

    #[test]
    fn the_example_stays_inside_the_folder_it_is_copied_to() {
        for folder in ["first", "second"] {
            let cfg = loaded(folder, str::to_string);
            let root = std::env::temp_dir().join(format!("liq2any-config-{}-{folder}", std::process::id()));
            assert_eq!(cfg.db, root.join("maker.db"));
            assert_eq!(cfg.signer_socket, root.join("run/signer.sock"));
            assert_eq!(cfg.pages, root.join("pages.log"));
            assert_eq!(cfg.rfq.unwrap().nostr_key, root.join("keys/nostr.key"));
        }
    }

    #[test]
    fn the_example_matches_the_amoy_deployment_and_the_engine_defaults() {
        let cfg = loaded("amoy", str::to_string);
        assert_eq!(cfg.polygon.escrow.as_deref(), Some("0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F"));
        assert_eq!(cfg.polygon.token.as_deref(), Some("0xb4236fbd3bc5791Ee0faB7f065f96F591d33a724"));
        // Amoy's escrow allows 604,800 blocks of extension: 7 days at its 1 s blocks.
        assert_eq!(cfg.engine, engine::Config { max_extension_blocks: 604_800, ..engine::Config::default() });
    }

    #[test]
    fn an_absolute_path_is_kept() {
        let cfg = loaded("absolute", |s| s.replace("\"maker.db\"", "\"/elsewhere/maker.db\""));
        assert_eq!(cfg.db, Path::new("/elsewhere/maker.db"));
    }

    #[test]
    fn a_lock_is_seen_only_when_it_exists_and_compared_with_the_terms() {
        let (address, recipient) = (EvmAddress([1; 20]), EvmAddress([2; 20]));
        let lock = Lock {
            initiator: EvmAddress([3; 20]),
            deadline_time: 10,
            state: LockState::Open,
            long_leg: true,
            extended: false,
            recipient,
            deadline_block: 20,
            lock_address: address,
            gas_drop: 0,
            amount: 5,
            revealed: [0; 32],
        };
        let v = escrow_view(&lock, &address, &recipient).unwrap();
        assert!(v.lock_address_ok && v.recipient_ok && v.long_leg && v.amount == 5);
        assert_eq!(escrow_view(&Lock { state: LockState::None, ..lock.clone() }, &address, &recipient), None);
        assert!(!escrow_view(&lock, &address, &EvmAddress([9; 20])).unwrap().recipient_ok);
        assert_eq!(escrow_view(&Lock { amount: u128::MAX, ..lock }, &address, &recipient).unwrap().amount, 0);
    }
}
