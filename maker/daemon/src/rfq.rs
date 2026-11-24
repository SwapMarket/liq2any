//! The maker's side of RFQ. Everything arriving here is attacker-reachable, so it is parsed strictly, rate limited, and
//! can only register a swap or take part in its signing session: no message moves funds, and the engine and signer still
//! check every lock on-chain and every signature share.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use liq2any_chain::esplora::Esplora;
use liq2any_chain::evm::{from_hex, Address as EvmAddress};
use liq2any_chain::liquid::usdt;
use liq2any_chain::polygon::{self, lock_id, Rpc as PolygonRpc, State as LockState};
use liq2any_maker_core::engine::{self, Terms};
use liq2any_maker_core::store::Store;
use liq2any_maker_core::LockPoint;
use liq2any_rfq::messages::*;
use liq2any_rfq::relay::{Relay, RelayEvent};
use liq2any_rfq::rules::{self, MakerRules, PairRules, RateLimit};
use liq2any_rfq::wrap;
use liq2any_signer::{Call, Client, Reply, SwapTerms};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{accept, lock_address, Details};

/// One direction the maker offers, in USDT0 base units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    pub from: Chain,
    pub to: Chain,
    pub min: u64,
    pub max: u64,
    /// What we charge, by size. `[{"up_to":"5000000","bps":30},{"up_to":"25000000","bps":40}]` charges 30 bps on swaps up to
    /// 5 USDT, and 40 above. The last step must reach `max`.
    pub prices: Vec<PriceTier>,
    /// The Liquid lock height, this many blocks ahead of acceptance.
    pub lock_blocks: u32,
    /// L→P only: the POL our lock forwards to the taker's `gas_to`, in wei, as a decimal string. At most the engine's
    /// `max_gas_drop`.
    #[serde(default)]
    pub gas_drop: Wei,
}

impl PairConfig {
    pub fn pair(&self) -> Pair {
        Pair { from: self.from, to: self.to }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfqConfig {
    /// The maker's identity. Not a funds key: losing it costs reputation, not money.
    pub nostr_key: PathBuf,
    pub relays: Vec<String>,
    pub network: String,
    pub pairs: Vec<PairConfig>,
    /// The most to admit to in an offer, for a maker who doesn't want to show its full capacity. It also limits what is quoted.
    #[serde(default)]
    pub show_available_up_to: Option<u64>,
    pub quote_secs: u64,
    pub offer_every_secs: u64,
    pub offer_ttl_secs: u64,
    pub max_requests_per_sender: usize,
    pub max_requests_per_window: usize,
    pub window_secs: u64,
    pub max_negotiations: usize,
}

impl RfqConfig {
    pub fn pairs(&self) -> &[PairConfig] {
        &self.pairs
    }
}

/// Why a pair list can't be used, or `None`. The engine's limits come first, because they bind every swap.
pub(crate) fn pairs_problem(pairs: &[PairConfig], engine: &engine::Config) -> Option<String> {
    let offered: Vec<PairOffer> = pairs
        .iter()
        .map(|p| PairOffer {
            from: p.from,
            to: p.to,
            min: Units(p.min),
            max: Units(p.max),
            available: Units(0),
            prices: p.prices.clone(),
        })
        .collect();
    if !rules::pairs_ok(&offered) {
        return Some("each pair must be Liquid to Polygon or back, listed once, with prices that rise in steps and reach its max".into());
    }
    for p in pairs {
        if p.max > engine.max_per_swap {
            return Some(format!("{:?}: max is above the engine's max_per_swap", p.pair()));
        }
        if p.pair().taker_pays_liquid() && p.gas_drop.0 > engine.max_gas_drop {
            return Some(format!("{:?}: the gas drop is above the engine's max_gas_drop", p.pair()));
        }
        if !p.pair().taker_pays_liquid() && p.gas_drop.0 != 0 {
            return Some(format!("{:?}: only L→P has a gas drop", p.pair()));
        }
    }
    None
}

fn direction_of(pair: Pair) -> engine::Direction {
    if pair.taker_pays_liquid() {
        engine::Direction::L2P
    } else {
        engine::Direction::P2L
    }
}

struct Negotiation {
    sender: PublicKey,
    pair: Pair,
    quote: Quote,
    quote_rumor_id: String,
    answered: Option<(String, Event)>,
}

pub struct Handler {
    cfg: RfqConfig,
    keys: Keys,
    store: Store,
    chain: Esplora,
    signer: Client,
    me: EvmAddress,
    fee_coin_sats: u64,
    engine: engine::Config,
    rate: RateLimit,
    negotiations: HashMap<String, Negotiation>,
    seen_events: HashSet<String>,
    recent: Vec<u64>,
    /// (Polygon, Liquid) that we hold, in USDT0 base units.
    balances: Arc<Mutex<(u64, u64)>>,
    polygon: PolygonRpc,
    second_polygon: Option<PolygonRpc>,
    ids: polygon::Ids,
}

/// Often enough for an offer, and rarely enough for a public API's rate limit.
const BALANCES_EVERY: Duration = Duration::from_secs(120);
const BALANCES_RETRY: Duration = Duration::from_secs(5);

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Load the identity key, creating it on first use with private permissions.
pub fn load_keys(path: &std::path::Path) -> Result<Keys, String> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        let keys = Keys::generate();
        std::fs::write(path, keys.secret_key().to_secret_hex()).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Keys::parse(text.trim()).map_err(|e| e.to_string())
}

impl Handler {
    pub fn new(cfg: RfqConfig, maker: &crate::Config) -> Result<Self, String> {
        if let Some(problem) = pairs_problem(&cfg.pairs, &maker.engine) {
            return Err(format!("refusing to start: {problem}"));
        }
        let keys = load_keys(&cfg.nostr_key)?;
        let store = Store::open(&maker.db).map_err(|e| e.to_string())?;
        let chain = crate::source(&maker.liquid);
        let mut signer = Client::connect(&maker.signer_socket)?;
        let Reply::Identity { polygon: me, fee_coin_sats, .. } = signer.call(&Call::Identity)? else {
            return Err("unexpected reply from the signer".into());
        };
        Ok(Self {
            cfg,
            keys,
            store,
            chain,
            signer,
            me: EvmAddress::parse(&me)?,
            fee_coin_sats,
            engine: maker.engine.clone(),
            rate: RateLimit::default(),
            negotiations: HashMap::new(),
            seen_events: HashSet::new(),
            recent: vec![],
            balances: Arc::default(),
            polygon: PolygonRpc::new(&maker.polygon.rpc),
            second_polygon: maker.polygon.second_rpc.as_deref().map(PolygonRpc::new),
            // Set by the main loop's start, which runs first and checks the chains.
            ids: polygon::ids(),
        })
    }

    /// What we can pay in each direction, less what open swaps are counting on. The balances come from a background reader,
    /// because reading them takes chain calls, and a taker asking for a quote shouldn't wait on that. Nothing read yet, or a
    /// failed read, shows nothing available.
    fn available(&mut self, pair: Pair) -> u64 {
        // Paused by the operator: nothing to offer, so pages route orders elsewhere, while open swaps carry on.
        if self.store.paused().unwrap_or(false) {
            return 0;
        }
        let (polygon, liquid) = self.balances.lock().map_or((0, 0), |b| *b);
        let direction = direction_of(pair);
        let open = self.store.open_swaps().unwrap_or_default();
        let committed: u64 = open
            .iter()
            .filter(|(t, s)| t.direction == direction && *s == engine::State::WatchLock)
            .map(|(t, _)| t.polygon_amount)
            .sum();
        // L→P pays out USDT0, P→L Liquid USDt.
        let have = if direction == engine::Direction::L2P { polygon } else { liquid };
        have.saturating_sub(committed).min(self.cfg.show_available_up_to.unwrap_or(u64::MAX))
    }

    /// L→P: how long our lock runs. P→L: the least the taker's lock must leave when we check it.
    fn window_secs(&self, pair: Pair) -> u64 {
        match direction_of(pair) {
            engine::Direction::L2P => self.engine.l2p_deadline_secs,
            engine::Direction::P2L => self.engine.p2l_gate_secs,
        }
    }

    fn rules(&mut self) -> MakerRules {
        let pairs = self.cfg.pairs.clone();
        MakerRules {
            network: self.cfg.network.clone(),
            pairs: pairs
                .iter()
                .map(|p| PairRules {
                    pair: p.pair(),
                    min: p.min,
                    max: p.max,
                    available: self.available(p.pair()),
                    prices: p.prices.clone(),
                    lock_blocks: p.lock_blocks,
                    polygon_window_secs: self.window_secs(p.pair()),
                    gas_drop: p.gas_drop.0,
                })
                .collect(),
            quote_secs: self.cfg.quote_secs,
            taker_lock_window: self.engine.l2p_fund_window_blocks,
        }
    }

    pub fn offer(&mut self, now: u64) -> Result<Event, String> {
        let pairs = self.cfg.pairs.clone();
        let offer = Offer {
            v: VERSION,
            network: self.cfg.network.clone(),
            template_version: TEMPLATE_VERSION,
            chains: Chains {
                liquid: LiquidInfo {
                    asset: usdt().to_string(),
                    confirmations: self.engine.confirmations,
                    fee_coin_sats: self.fee_coin_sats,
                },
                polygon: PolygonInfo {
                    chain_id: self.ids.chain_id,
                    escrow: self.ids.escrow.checksummed(),
                    token: self.ids.token.checksummed(),
                    max_gas_drop: Wei(pairs.iter().map(|p| p.gas_drop.0).max().unwrap_or(0)),
                },
            },
            pairs: pairs
                .iter()
                .map(|p| PairOffer {
                    from: p.from,
                    to: p.to,
                    min: Units(p.min),
                    max: Units(p.max),
                    available: Units(self.available(p.pair())),
                    prices: p.prices.clone(),
                })
                .collect(),
            relays: self.cfg.relays.clone(),
        };
        wrap::offer_event(&self.keys, &offer, now, self.cfg.offer_ttl_secs).map_err(|e| format!("{e:?}"))
    }

    fn reply(&self, to: &PublicKey, id: &str, body: Body, now: u64) -> Result<(Event, String), String> {
        let envelope =
            Envelope { v: VERSION, id: id.to_string(), network: self.cfg.network.clone(), sent_at: now, body };
        wrap::wrap(&self.keys, to, &envelope, now).map_err(|e| format!("{e:?}"))
    }

    /// Everything else in this file serves this: turn one event into at most one answer.
    pub fn on_event(&mut self, event: &Event, now: u64) -> Option<Event> {
        // The cheapest check first, so a flood of copies costs almost nothing.
        if !self.seen_events.insert(event.id.to_hex()) {
            return None;
        }
        if self.seen_events.len() > 100_000 {
            self.seen_events.clear();
        }
        let got = wrap::unwrap(&self.keys, event).ok()?;
        rules::check_envelope(&got.envelope, &self.cfg.network, now).ok()?;
        let sender = got.sender.to_hex();
        if !self.rate.allow(&sender, now, self.cfg.window_secs, self.cfg.max_requests_per_sender) {
            return None;
        }
        self.recent.retain(|t| now.saturating_sub(*t) < self.cfg.window_secs);
        if self.recent.len() >= self.cfg.max_requests_per_window {
            return None;
        }
        self.recent.push(now);
        let id = got.envelope.id.clone();
        match got.envelope.body {
            Body::RfqRequest(request) => self.on_request(&got.sender, &id, request, now),
            Body::RfqAccept(acc) => self.on_accept(&got.sender, &id, &got.rumor_id, acc, now),
            Body::ClaimAnswer(a) => self.on_claim_answer(&got.sender, &id, a),
            Body::EarlyRefundAsk(a) => self.on_early_refund(&got.sender, &id, a, now),
            Body::ReleaseAsk(a) => self.on_release(&got.sender, &id, a, now),
            Body::CancelAsk(a) => self.on_cancel(&got.sender, &id, a, now),
            Body::SessionOpen(o) => self.on_session_open(&got.sender, &id, o, now),
            Body::TakerShare(s) => self.on_taker_share(&got.sender, &id, s, now),
            // A taker has no business sending these, and a reject has nothing to free up in this version.
            Body::RfqQuote(_)
            | Body::RfqTerms(_)
            | Body::RfqReject(_)
            | Body::ClaimOffer(_)
            | Body::SessionNonce(_)
            | Body::MakerShare(_)
            | Body::EarlyRefundShare(_)
            | Body::Cancelled(_) => None,
        }
    }

    fn on_request(&mut self, sender: &PublicKey, id: &str, request: Request, now: u64) -> Option<Event> {
        if self.negotiations.contains_key(id) {
            return None;
        }
        self.negotiations.retain(|_, n| now <= n.quote.quote_expiry + 600);
        if self.negotiations.len() >= self.cfg.max_negotiations {
            return None;
        }
        if self.store.paused().unwrap_or(false) {
            let body = Body::RfqReject(Reject { reason: "this maker is paused and takes no new swaps for now".into() });
            return self.reply(sender, id, body, now).ok().map(|(event, _)| event);
        }
        let rules = self.rules();
        // Say so, or the taker can only time out and can't tell a refusal from a maker that is gone.
        let Ok(quote) = rules::make_quote(&rules, &request, now) else {
            let body = Body::RfqReject(Reject { reason: "this maker can't quote that amount right now".into() });
            return self.reply(sender, id, body, now).ok().map(|(event, _)| event);
        };
        let (event, quote_rumor_id) = self.reply(sender, id, Body::RfqQuote(quote.clone()), now).ok()?;
        self.negotiations.insert(
            id.to_string(),
            Negotiation { sender: *sender, pair: request.pair(), quote, quote_rumor_id, answered: None },
        );
        Some(event)
    }

    fn on_accept(&mut self, sender: &PublicKey, id: &str, rumor_id: &str, acc: Accept, now: u64) -> Option<Event> {
        let n = self.negotiations.get(id)?;
        // Only the key that asked may accept, so nobody can hijack a quote.
        if n.sender != *sender {
            return None;
        }
        // The same accept delivered again gets the same answer, in case the first was lost.
        if let Some((answered_id, event)) = &n.answered {
            return (answered_id == rumor_id).then(|| event.clone());
        }
        let checked = rules::check_accept(&acc, &n.quote, &n.quote_rumor_id, now).ok()?;
        // A quote given before the pause doesn't start a swap after it.
        if self.store.paused().unwrap_or(false) {
            let body = Body::RfqReject(Reject { reason: "this maker is paused and takes no new swaps for now".into() });
            return self.reply(sender, id, body, now).ok().map(|(e, _)| e);
        }
        let (pair, quote) = (n.pair, n.quote.clone());
        match self.terms_for(sender, id, rumor_id, pair, &quote, checked, now) {
            Ok(event) => {
                if let Some(n) = self.negotiations.get_mut(id) {
                    n.answered = Some((rumor_id.to_string(), event.clone()));
                }
                Some(event)
            }
            Err(reason) => {
                eprintln!("rfq: accept refused: {}", reason.chars().take(200).collect::<String>());
                let body = Body::RfqReject(Reject { reason: "this swap can't be accepted".into() });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn terms_for(
        &mut self,
        sender: &PublicKey,
        id: &str,
        accept_rumor_id: &str,
        pair: Pair,
        quote: &Quote,
        c: rules::CheckedAccept,
        now: u64,
    ) -> Result<Event, String> {
        let p = self.cfg.pairs.iter().find(|p| p.pair() == pair).ok_or("a pair we don't offer")?.clone();
        let direction = direction_of(pair);
        let tip = self.chain.tip()?;
        let (send, receive) = (quote.send.0, quote.receive.0);
        let (liquid_amount, polygon_amount) = match direction {
            engine::Direction::L2P => (send.checked_mul(100), receive),
            engine::Direction::P2L => (receive.checked_mul(100), send),
        };
        let liquid_amount = liquid_amount.ok_or("amount overflow")?;
        let lock_height = tip.checked_add(p.lock_blocks).ok_or("height overflow")?;
        // A bad address, or a bad checksum, is refused here, and not when it would later fail on-chain. Both are kept
        // checksummed, so every later reading of them agrees.
        let taker_polygon = EvmAddress::parse(&c.taker_polygon)?;
        if taker_polygon == self.me || taker_polygon == self.ids.escrow || taker_polygon == self.ids.token {
            return Err("the taker's Polygon address is one of ours".into());
        }
        let gas_to = c.gas_to.as_deref().map(EvmAddress::parse).transpose()?;
        // The quote's drop: it is zero for P→L, and the accept named a destination exactly when it isn't.
        let gas_drop = quote.gas_drop.0;

        let lock_point = LockPoint(c.lock_point);
        let Reply::SwapKey { key } = self.signer.call(&Call::SwapKey { swap_id: lock_point.id() })? else {
            return Err("unexpected reply from the signer".into());
        };
        let swap = SwapTerms {
            direction,
            lock_point,
            taker_key: LockPoint(c.taker_key),
            blinding_key: c.blinding_key,
            lock_height,
            liquid_amount,
            polygon_amount,
            taker_polygon: taker_polygon.checksummed(),
            gas_to: gas_to.map(|a| a.checksummed()),
            gas_drop,
        };
        // A point off the curve, a bad key or a bad blinding key is refused here, and not when funds depend on it.
        lock_address(&lock_point)?;
        swap.address(&key)?;
        let details = Details { terms: swap.clone(), taker_nostr: sender.to_hex(), negotiation: id.to_string() };
        let terms =
            Terms { direction, lock_point, liquid_amount, polygon_amount, h_accept: tip, lock_height, gas_drop };
        // The forever check: a point ever seen before is refused, because its secret may be public.
        if !accept(&self.store, &terms, &details)? {
            return Err("this lock point has been used before".into());
        }
        let body = Body::RfqTerms(MakerTerms {
            accept_event: accept_rumor_id.to_string(),
            h_accept: tip,
            lock_height,
            maker_key: key.to_hex(),
            maker_polygon: self.me.checksummed(),
            usdt_asset: usdt().to_string(),
            chain_id: self.ids.chain_id,
            escrow: self.ids.escrow.checksummed(),
            token: self.ids.token.checksummed(),
            template_version: TEMPLATE_VERSION,
            liquid_amount: Units(liquid_amount),
            polygon_amount: Units(polygon_amount),
            polygon_window_secs: self.window_secs(pair),
            gas_drop: Wei(gas_drop),
        });
        self.reply(sender, id, body, now).map(|(event, _)| event)
    }

    /// Send a signed Polygon transaction through every endpoint.
    fn send_polygon(&self, reply: Reply) -> Result<(), String> {
        let Reply::Polygon { raw, .. } = reply else {
            return Err(format!("{reply:?}"));
        };
        let raw = from_hex(&raw)?;
        let first = self.polygon.send_raw(&raw).map(|_| ());
        if let Some(second) = &self.second_polygon {
            let _ = second.send_raw(&raw);
        }
        first
    }

    /// The swap a session message belongs to. Only that swap's taker, in that swap's negotiation, gets anywhere, and
    /// nobody does while the rollback alarm is on: what we believe about the chain can't be trusted then.
    fn swap_for(&self, sender: &PublicKey, id: &str, lock_point: &str) -> Option<SwapTerms> {
        if self.store.rollback().unwrap_or(true) {
            return None;
        }
        let point = LockPoint::from_hex(lock_point)?;
        let body = self.store.details(&point.id()).ok()??;
        let details: Details = serde_json::from_str(&body).ok()?;
        (details.taker_nostr == sender.to_hex() && details.negotiation == id && details.terms.lock_point == point)
            .then_some(details.terms)
    }

    /// L→P. Nothing goes back: the engine sees the stored signature on its next tick, and locks.
    fn on_claim_answer(&mut self, sender: &PublicKey, id: &str, a: ClaimAnswer) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        let call = Call::StoreAdaptor { terms, taker_nonce: a.nonce, taker_share: a.share };
        match self.signer.call(&call) {
            Ok(Reply::Held(true)) => println!("{} adapter signature stored", a.lock_point),
            other => eprintln!("rfq: claim answer refused: {other:?}"),
        }
        None
    }

    /// L→P: the signer promises never to lock, or refuses because it already has. The refund is signed later, on request.
    /// P→L: see `cancel_p2l`.
    fn on_cancel(&mut self, sender: &PublicKey, id: &str, a: CancelAsk, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        if terms.direction == engine::Direction::P2L {
            return self.cancel_p2l(sender, id, terms, a, now);
        }
        let swap = terms.lock_point.id();
        let body = match self.signer.call(&Call::Cancel { terms }) {
            Ok(Reply::Nothing) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                }
                println!("{} cancelled, at the taker's request", a.lock_point);
                Body::Cancelled(Cancelled { lock_point: a.lock_point })
            }
            other => {
                eprintln!("rfq: cancel refused: {other:?}");
                Body::RfqReject(Reject { reason: "this maker can't cancel now".into() })
            }
        };
        self.reply(sender, id, body, now).ok().map(|(e, _)| e)
    }

    /// P→L, before the taker locks: the taker cancelled, so give the swap up now rather than at the end of its lock window, which
    /// frees what it held of our Liquid for other takers. We lock only after seeing the taker's lock, so nothing of ours is promised
    /// and the signer isn't asked. Refused once the taker has locked, even unfinalized, or when Polygon can't be read, since the
    /// swap must go on then.
    fn cancel_p2l(&mut self, sender: &PublicKey, id: &str, terms: SwapTerms, a: CancelAsk, now: u64) -> Option<Event> {
        let swap = terms.lock_point.id();
        let waiting = matches!(self.store.state(&swap), Ok(Some((_, engine::State::WatchLock))));
        let taker_locked = (|| -> Result<bool, String> {
            let id = lock_id(&terms.taker()?, &lock_address(&terms.lock_point)?);
            Ok(self.polygon.lock_at(&self.ids.escrow, &id, "latest")?.state != LockState::None)
        })();
        let body = match (waiting, taker_locked) {
            (true, Ok(false)) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                    return None;
                }
                println!("{} cancelled before the taker locked, at the taker's request", a.lock_point);
                Body::Cancelled(Cancelled { lock_point: a.lock_point })
            }
            (_, Ok(true)) => Body::RfqReject(Reject { reason: "the taker has locked already".into() }),
            (_, Err(e)) => {
                eprintln!("rfq: cancel of {}: can't read Polygon: {e}", a.lock_point);
                Body::RfqReject(Reject { reason: "this maker can't cancel now".into() })
            }
            (false, _) => Body::RfqReject(Reject { reason: "this maker can't cancel now".into() }),
        };
        self.reply(sender, id, body, now).ok().map(|(e, _)| e)
    }

    /// L→P. The signer decides, and if it agrees it has already promised never to lock.
    fn on_early_refund(&mut self, sender: &PublicKey, id: &str, a: EarlyRefundAsk, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        let swap = terms.lock_point.id();
        let call = Call::EarlyRefund { terms, tx: a.tx, taker_nonce: a.nonce };
        let body = match self.signer.call(&call) {
            Ok(Reply::EarlyRefundShare { nonce, share }) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                }
                println!("{} refunded early, at the taker's request", a.lock_point);
                Body::EarlyRefundShare(EarlyRefundShare { lock_point: a.lock_point, nonce, share })
            }
            other => {
                eprintln!("rfq: early refund refused: {other:?}");
                Body::RfqReject(Reject { reason: "this maker can't refund early now".into() })
            }
        };
        self.reply(sender, id, body, now).ok().map(|(e, _)| e)
    }

    /// P→L. The signer refuses once we have locked on Liquid, and promises never to afterwards. The escrow sends the tokens
    /// back to the lock's own initiator, so nothing here can redirect them. Asking again sends the release again.
    fn on_release(&mut self, sender: &PublicKey, id: &str, a: ReleaseAsk, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &a.lock_point)?;
        if terms.direction != engine::Direction::P2L {
            return None;
        }
        let swap = terms.lock_point.id();
        let call = Call::Release { taker_polygon: terms.taker_polygon.clone(), lock_point: terms.lock_point };
        let sent = self.signer.call(&call).and_then(|reply| self.send_polygon(reply));
        match sent {
            Ok(()) => {
                if let Err(e) = self.store.commit_step(&swap, &engine::State::Abandoned, &[]) {
                    eprintln!("rfq: couldn't mark {} abandoned: {e}", a.lock_point);
                }
                println!("{} released to the taker, at the taker's request", a.lock_point);
                None
            }
            Err(e) => {
                eprintln!("rfq: release refused: {}", e.chars().take(200).collect::<String>());
                let body = Body::RfqReject(Reject { reason: "this maker can't refund early now".into() });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
        }
    }

    fn on_session_open(&mut self, sender: &PublicKey, id: &str, o: SessionOpen, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &o.lock_point)?;
        let call = Call::OpenSession { terms, claim_tx: o.tx, taker_nonce: o.nonce };
        match self.signer.call(&call) {
            Ok(Reply::Nonce(nonce)) => {
                let body = Body::SessionNonce(SessionNonce { lock_point: o.lock_point, nonce });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
            other => {
                eprintln!("rfq: signing session refused: {other:?}");
                None
            }
        }
    }

    fn on_taker_share(&mut self, sender: &PublicKey, id: &str, s: SessionShare, now: u64) -> Option<Event> {
        let terms = self.swap_for(sender, id, &s.lock_point)?;
        match self.signer.call(&Call::FinishSession { terms, taker_share: s.share }) {
            Ok(Reply::Share(share)) => {
                let body = Body::MakerShare(SessionShare { lock_point: s.lock_point, share });
                self.reply(sender, id, body, now).ok().map(|(e, _)| e)
            }
            other => {
                eprintln!("rfq: taker's share refused: {other:?}");
                None
            }
        }
    }
}

/// How the main loop reaches a taker. Everything goes out through every relay.
#[derive(Clone)]
pub struct Outbox {
    keys: Keys,
    network: String,
    relays: Vec<Sender<Event>>,
}

impl Outbox {
    pub fn send(&self, to_hex: &str, id: &str, body: Body) -> Result<(), String> {
        let to = PublicKey::from_hex(to_hex).map_err(|e| e.to_string())?;
        let now = now_secs();
        let envelope = Envelope { v: VERSION, id: id.to_string(), network: self.network.clone(), sent_at: now, body };
        let (event, _) = wrap::wrap(&self.keys, &to, &envelope, now).map_err(|e| format!("{e:?}"))?;
        for relay in &self.relays {
            // A relay thread only goes away with the process, so a failure here means nothing is listening anyway.
            let _ = relay.send(event.clone());
        }
        Ok(())
    }
}

/// One relay, forever: reconnect on failure, publish offers, and answer what arrives. A relay that goes away costs
/// quotes on that relay, and nothing else.
fn run_relay(
    url: String,
    handler: Arc<Mutex<Handler>>,
    pubkey: PublicKey,
    offer_every: Duration,
    balances_changed: Arc<AtomicU64>,
    outgoing: Receiver<Event>,
) {
    loop {
        let mut relay = match Relay::connect(&url, Duration::from_secs(1)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("rfq: {e}");
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        // Wrappers carry randomized timestamps up to two days old, so a shorter window would lose messages.
        let since = now_secs().saturating_sub(2 * 86_400);
        if let Err(e) = relay.subscribe("liq2any-maker", &[Kind::from(WRAP_KIND)], Some(&pubkey), since) {
            eprintln!("rfq: {url}: {e}");
            continue;
        }
        let mut last_offer: Option<Instant> = None;
        let mut offered_for = balances_changed.load(Ordering::SeqCst);
        // Whatever piled up during an outage is stale, and the engine will ask again.
        while outgoing.try_recv().is_ok() {}
        'connected: loop {
            while let Ok(event) = outgoing.try_recv() {
                if let Err(e) = relay.publish(&event) {
                    eprintln!("rfq: {url}: {e}");
                    break 'connected;
                }
            }
            let balances_now = balances_changed.load(Ordering::SeqCst);
            if last_offer.is_none_or(|t| t.elapsed() >= offer_every) || balances_now != offered_for {
                offered_for = balances_now;
                let offer = handler.lock().map_err(|_| "poisoned".to_string()).and_then(|mut h| h.offer(now_secs()));
                match offer.and_then(|o| relay.publish(&o)) {
                    Ok(()) => last_offer = Some(Instant::now()),
                    Err(e) => {
                        eprintln!("rfq: {url}: {e}");
                        break;
                    }
                }
            }
            match relay.next_event() {
                Ok(Some(RelayEvent::Event(event))) => {
                    let answer = handler.lock().ok().and_then(|mut h| h.on_event(&event, now_secs()));
                    if let Some(answer) = answer {
                        if let Err(e) = relay.publish(&answer) {
                            eprintln!("rfq: {url}: {e}");
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("rfq: {url}: {e}");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// One read of what we hold. A failure reads as nothing, so we don't quote on stale funds. A change bumps `changed`, which has
/// every relay publish a new offer at once, rather than at its next turn: funds just arrived, or ran out.
fn read_balances(signer: &mut Client, balances: &Mutex<(u64, u64)>, changed: &AtomicU64) -> bool {
    let (read, ok) = match signer.call(&Call::Capacity) {
        Ok(Reply::Capacity { polygon_units, liquid_units }) => ((polygon_units, liquid_units / 100), true),
        other => {
            eprintln!("rfq: could not read our balances: {other:?}");
            ((0, 0), false)
        }
    };
    if let Ok(mut b) = balances.lock() {
        if *b != read {
            changed.fetch_add(1, Ordering::SeqCst);
        }
        *b = read;
    }
    ok
}

/// Start answering on every configured relay.
pub fn start(cfg: RfqConfig, maker: &crate::Config) -> Result<(PublicKey, Outbox), String> {
    let relays = cfg.relays.clone();
    let every = Duration::from_secs(cfg.offer_every_secs);
    let handler = Handler::new(cfg, maker)?;
    let pubkey = handler.keys.public_key();
    let mut outbox = Outbox { keys: handler.keys.clone(), network: handler.cfg.network.clone(), relays: vec![] };
    // Once before any relay starts, so the first offer is right, and then in the background, on a connection of its own.
    let mut reader = Client::connect(&maker.signer_socket)?;
    let balances = handler.balances.clone();
    let changed = Arc::new(AtomicU64::new(0));
    read_balances(&mut reader, &balances, &changed);
    let bump = changed.clone();
    // A failed read, such as the first after the signer restarts, is tried again soon, so the offer isn't left at nothing.
    std::thread::spawn(move || {
        let mut wait = BALANCES_EVERY;
        loop {
            std::thread::sleep(wait);
            wait = if read_balances(&mut reader, &balances, &bump) { BALANCES_EVERY } else { BALANCES_RETRY };
        }
    });
    let handler = Arc::new(Mutex::new(handler));
    for url in relays {
        let h = handler.clone();
        let (send, receive) = channel();
        outbox.relays.push(send);
        let changed = changed.clone();
        std::thread::spawn(move || run_relay(url, h, pubkey, every, changed, receive));
    }
    Ok((pubkey, outbox))
}
