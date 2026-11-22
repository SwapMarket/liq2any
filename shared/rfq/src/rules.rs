//! What each side must check. Pure functions, so every rule can be tested without a relay. From liq2sol's, with pairs of
//! chains and Polygon's fields (docs/design.md section 5).

use crate::messages::*;

/// How far a message's `sent_at` may be from our clock. NIP-59 randomizes the outer timestamps, so this is the only
/// freshness signal, and it is inside the encrypted payload.
pub const MAX_CLOCK_SKEW_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    WrongVersion,
    WrongNetwork,
    BadId,
    Stale,
    UnexpectedMessage,
    UnknownNegotiation,
    QuoteExpired,
    /// The accept names a quote we didn't send, or the terms name an accept we didn't send.
    WrongReference,
    Malformed(&'static str),
    OutOfLimits,
    NotMultipleOf100,
    /// We wouldn't receive what was agreed, or would pay more.
    BadAmounts,
    BadLockHeight,
    /// A Polygon window outside what the taker accepts.
    BadWindow,
    /// The escrow, token or chain isn't the one pinned for this network.
    WrongContract,
    WrongAsset,
    WrongTemplate,
    /// A direction with no Liquid side, or one this maker doesn't fill.
    WrongPair,
    DuplicateHash,
    RateLimited,
}

/// Checks every message needs, on either side.
pub fn check_envelope(e: &Envelope, network: &str, now: u64) -> Result<(), Refused> {
    if e.v != VERSION {
        return Err(Refused::WrongVersion);
    }
    if e.network != network {
        return Err(Refused::WrongNetwork);
    }
    if hex32(&e.id).is_none() {
        return Err(Refused::BadId);
    }
    if e.sent_at.abs_diff(now) > MAX_CLOCK_SKEW_SECS {
        return Err(Refused::Stale);
    }
    Ok(())
}

/// What a maker offers in one direction, in USDT0 base units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRules {
    pub pair: Pair,
    pub min: u64,
    pub max: u64,
    /// What the maker can pay right now, less what open swaps are counting on.
    pub available: u64,
    pub prices: Vec<PriceTier>,
    /// The Liquid lock height, this many blocks ahead of acceptance.
    pub lock_blocks: u32,
    /// L→P: how long the maker's Polygon lock runs. P→L: the least the taker's lock must leave on both clocks.
    pub polygon_window_secs: u64,
    /// L→P: the POL the maker's lock forwards. Zero for P→L.
    pub gas_drop: u128,
}

/// The maker's pricing and limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MakerRules {
    pub network: String,
    pub pairs: Vec<PairRules>,
    pub quote_secs: u64,
    pub taker_lock_window: u32,
}

impl MakerRules {
    pub fn pair(&self, pair: Pair) -> Option<&PairRules> {
        self.pairs.iter().find(|p| p.pair == pair)
    }
}

/// (send, receive) for a request, always rounding in the maker's favour. `None` if it can't be priced.
pub fn price(request: &Request, bps: u32) -> Option<(u64, u64)> {
    let amount = u128::from(request.amount.0);
    let keep = 10_000u128.checked_sub(u128::from(bps)).filter(|k| *k > 0)?;
    let (send, receive) = match request.amount_side {
        // Sending `amount`: the taker receives it less the spread, rounded down.
        AmountSide::Send => (amount, amount.checked_mul(keep)? / 10_000),
        // Receiving `amount`: the taker sends it plus the spread, rounded up.
        AmountSide::Receive => (amount.checked_mul(10_000)?.div_ceil(keep), amount),
    };
    if send == 0 || receive == 0 {
        return None;
    }
    Some((u64::try_from(send).ok()?, u64::try_from(receive).ok()?))
}

pub const MAX_PRICE_TIERS: usize = 16;
pub const MAX_PAIRS: usize = 8;

/// Whether a list of prices can be used: steps in rising order, each a possible spread, and covering every size up to `max`.
pub fn prices_ok(tiers: &[PriceTier], max: u64) -> bool {
    !tiers.is_empty()
        && tiers.len() <= MAX_PRICE_TIERS
        && tiers.iter().all(|t| t.bps < 10_000)
        && tiers.windows(2).all(|w| w[0].up_to < w[1].up_to)
        && tiers.last().is_some_and(|t| t.up_to.0 >= max)
}

/// Whether an offer's pairs can be used: valid directions, each listed once, with usable prices and limits.
pub fn pairs_ok(pairs: &[PairOffer]) -> bool {
    !pairs.is_empty()
        && pairs.len() <= MAX_PAIRS
        && pairs.iter().all(|p| p.pair().is_valid() && p.min <= p.max && prices_ok(&p.prices, p.max.0))
        && pairs.iter().enumerate().all(|(i, p)| pairs[..i].iter().all(|q| q.pair() != p.pair()))
}

/// The spread for this request: the first tier covering what the taker would send.
pub fn bps_for(tiers: &[PriceTier], request: &Request) -> Option<u32> {
    tiers.iter().find(|t| price(request, t.bps).is_some_and(|(send, _)| send <= t.up_to.0)).map(|t| t.bps)
}

/// The maker's answer to a request.
pub fn make_quote(r: &MakerRules, request: &Request, now: u64) -> Result<Quote, Refused> {
    let pair = request.pair();
    if !pair.is_valid() {
        return Err(Refused::WrongPair);
    }
    let p = r.pair(pair).ok_or(Refused::WrongPair)?;
    let bps = bps_for(&p.prices, request).ok_or(Refused::OutOfLimits)?;
    let (send, receive) = price(request, bps).ok_or(Refused::OutOfLimits)?;
    // The minimum is what the taker sends, so an order of exactly the minimum fills (liq2sol's user decision, 2026-11-18). The
    // maximum and the mainnet cap still hold for both sides.
    if send.max(receive) > p.max || send < p.min {
        return Err(Refused::OutOfLimits);
    }
    if send.checked_mul(100).is_none() || receive.checked_mul(100).is_none() {
        return Err(Refused::OutOfLimits);
    }
    // The maker pays `receive`. A quote it can't fill would tie up the taker's funds until a refund.
    if receive > p.available {
        return Err(Refused::OutOfLimits);
    }
    let gas_drop = if pair.taker_pays_liquid() { p.gas_drop } else { 0 };
    Ok(Quote {
        send: Units(send),
        receive: Units(receive),
        quote_expiry: now + r.quote_secs,
        taker_lock_window: r.taker_lock_window,
        gas_drop: Wei(gas_drop),
    })
}

/// An accept the maker has checked, with its fields decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAccept {
    pub lock_point: [u8; 33],
    pub taker_key: [u8; 33],
    pub blinding_key: [u8; 32],
    pub taker_polygon: String,
    pub gas_to: Option<String>,
}

/// `0x` and 40 hex digits. The checksum, when there is one, is checked where the address is used.
pub fn plausible_evm(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// The zero address, which can't receive and must never be a party.
fn is_zero(s: &str) -> bool {
    s[2..].bytes().all(|b| b == b'0')
}

/// The maker's check of an accept. It must also refuse any point it has ever seen, in its store.
pub fn check_accept(accept: &Accept, quote: &Quote, quote_rumor_id: &str, now: u64) -> Result<CheckedAccept, Refused> {
    if accept.quote_event != quote_rumor_id {
        return Err(Refused::WrongReference);
    }
    if now > quote.quote_expiry {
        return Err(Refused::QuoteExpired);
    }
    let lock_point = hex33(&accept.lock_point).ok_or(Refused::Malformed("lock_point"))?;
    let taker_key = hex33(&accept.taker_key).ok_or(Refused::Malformed("taker_key"))?;
    let blinding_key = hex32(&accept.blinding_key).ok_or(Refused::Malformed("blinding_key"))?;
    if !plausible_evm(&accept.taker_polygon) || is_zero(&accept.taker_polygon) {
        return Err(Refused::Malformed("taker_polygon"));
    }
    // A gas drop needs somewhere to go, and there is none to direct without one.
    match (&accept.gas_to, quote.gas_drop.0) {
        (Some(to), drop) if drop > 0 && plausible_evm(to) && !is_zero(to) => {}
        (None, 0) => {}
        _ => return Err(Refused::Malformed("gas_to")),
    }
    Ok(CheckedAccept {
        lock_point,
        taker_key,
        blinding_key,
        taker_polygon: accept.taker_polygon.clone(),
        gas_to: accept.gas_to.clone(),
    })
}

/// What a taker holds to, pinned per network in the released code. Nothing here comes from the maker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakerRules {
    pub network: String,
    pub usdt_asset: String,
    pub chain_id: u64,
    /// Lowercase, so a checksummed spelling still matches.
    pub escrow: String,
    pub token: String,
    pub template_version: u32,
    /// L→P: the taker's funds are locked until this far ahead. Near enough not to tie them up, far enough for the swap.
    pub l2p_min_blocks: u32,
    pub l2p_max_blocks: u32,
    /// P→L: how long the taker has to claim on Liquid.
    pub p2l_min_blocks: u32,
    pub p2l_max_blocks: u32,
    /// L→P: the maker's Polygon lock must give the taker at least this long to claim, and no more than the most.
    pub l2p_min_window_secs: u64,
    pub l2p_max_window_secs: u64,
    /// P→L: the most the maker may ask the taker's lock to leave, which sets how long the taker's own funds are tied up.
    pub p2l_max_window_secs: u64,
    /// The worst spread the taker accepts, in basis points.
    pub max_spread_bps: u32,
}

/// The taker's check of a quote against what it asked for.
pub fn check_quote(r: &TakerRules, request: &Request, quote: &Quote, now: u64) -> Result<(), Refused> {
    if now > quote.quote_expiry {
        return Err(Refused::QuoteExpired);
    }
    let (send, receive) = (quote.send.0, quote.receive.0);
    let asked = match request.amount_side {
        AmountSide::Send => send == request.amount.0,
        AmountSide::Receive => receive == request.amount.0,
    };
    if !asked || receive == 0 || receive > send {
        return Err(Refused::BadAmounts);
    }
    // Spread = (send - receive) / send, compared without division.
    let lost = u128::from(send - receive) * 10_000;
    if lost > u128::from(send) * u128::from(r.max_spread_bps) {
        return Err(Refused::BadAmounts);
    }
    if !request.pair().taker_pays_liquid() && quote.gas_drop.0 != 0 {
        return Err(Refused::BadAmounts);
    }
    Ok(())
}

/// The taker's last check before funding. `tip` is from the taker's own chain view.
pub fn check_terms(
    r: &TakerRules,
    pair: Pair,
    quote: &Quote,
    accept_rumor_id: &str,
    terms: &MakerTerms,
    tip: u32,
) -> Result<(), Refused> {
    if !pair.is_valid() {
        return Err(Refused::WrongPair);
    }
    if terms.accept_event != accept_rumor_id {
        return Err(Refused::WrongReference);
    }
    if terms.chain_id != r.chain_id
        || terms.escrow.to_ascii_lowercase() != r.escrow.to_ascii_lowercase()
        || terms.token.to_ascii_lowercase() != r.token.to_ascii_lowercase()
    {
        return Err(Refused::WrongContract);
    }
    if terms.usdt_asset != r.usdt_asset {
        return Err(Refused::WrongAsset);
    }
    if terms.template_version != r.template_version {
        return Err(Refused::WrongTemplate);
    }
    let (send, receive) = (quote.send.0, quote.receive.0);
    let (liquid, polygon) = if pair.taker_pays_liquid() {
        // The taker pays Liquid and receives Polygon.
        (send.checked_mul(100), receive)
    } else {
        // The taker pays Polygon and receives Liquid.
        (receive.checked_mul(100), send)
    };
    if Some(terms.liquid_amount.0) != liquid || terms.polygon_amount.0 != polygon || terms.gas_drop != quote.gas_drop {
        return Err(Refused::BadAmounts);
    }
    if !terms.liquid_amount.0.is_multiple_of(100) {
        return Err(Refused::NotMultipleOf100);
    }
    let blocks = terms.lock_height.saturating_sub(tip);
    let (min, max) =
        if pair.taker_pays_liquid() { (r.l2p_min_blocks, r.l2p_max_blocks) } else { (r.p2l_min_blocks, r.p2l_max_blocks) };
    if blocks < min || blocks > max {
        return Err(Refused::BadLockHeight);
    }
    let window_ok = if pair.taker_pays_liquid() {
        (r.l2p_min_window_secs..=r.l2p_max_window_secs).contains(&terms.polygon_window_secs)
    } else {
        terms.polygon_window_secs <= r.p2l_max_window_secs
    };
    if !window_ok {
        return Err(Refused::BadWindow);
    }
    if hex33(&terms.maker_key).is_none() || !plausible_evm(&terms.maker_polygon) || is_zero(&terms.maker_polygon) {
        return Err(Refused::Malformed("maker keys"));
    }
    Ok(())
}

/// Per-sender rate limiting. Sender keys are free, so this only raises the cost: the maker's caps are the real bound.
#[derive(Debug, Default)]
pub struct RateLimit {
    hits: std::collections::HashMap<String, Vec<u64>>,
}

impl RateLimit {
    /// Allow at most `max` per `window` seconds for this sender. Also caps how many senders are remembered.
    pub fn allow(&mut self, sender: &str, now: u64, window: u64, max: usize) -> bool {
        const MAX_SENDERS: usize = 10_000;
        if self.hits.len() >= MAX_SENDERS && !self.hits.contains_key(sender) {
            self.hits.retain(|_, times| times.iter().any(|t| now.saturating_sub(*t) < window));
            if self.hits.len() >= MAX_SENDERS {
                return false;
            }
        }
        let times = self.hits.entry(sender.to_string()).or_default();
        times.retain(|t| now.saturating_sub(*t) < window);
        if times.len() >= max {
            return false;
        }
        times.push(now);
        true
    }
}
