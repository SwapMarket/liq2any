//! The wire format, version 3: docs/design.md section 5. Unknown fields are refused, so a message means exactly what both sides
//! think it means. From liq2sol's version 2, with directions as pairs of chains.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const VERSION: u32 = 3;
/// Which swap contracts this version uses: the confidential ones, in docs/confidential.md.
pub const TEMPLATE_VERSION: u32 = 2;
pub const OFFER_KIND: u16 = 38401;
pub const RUMOR_KIND: u16 = 28401;
pub const WRAP_KIND: u16 = 21059;
pub const SEAL_KIND: u16 = 13;
pub const TOPIC: &str = "liq2any";
/// The largest wrapped event either side accepts. The largest real one carries a blinded claim, about 20 KB once wrapped.
pub const MAX_EVENT_BYTES: usize = 32 * 1024;

/// A base-unit amount, as a decimal string so it survives JSON parsers that lose precision on large numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Units(pub u64);

impl Serialize for Units {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Units {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        // Digits only: no sign, no spaces, no exponent, and no leading zeros to smuggle in a second spelling.
        let canonical =
            !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && (text == "0" || !text.starts_with('0'));
        if !canonical {
            return Err(serde::de::Error::custom("amounts are plain decimal strings"));
        }
        text.parse().map(Units).map_err(serde::de::Error::custom)
    }
}

/// A wei amount, which can pass 64 bits, as a decimal string like `Units`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Wei(pub u128);

impl Serialize for Wei {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Wei {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let canonical =
            !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && (text == "0" || !text.starts_with('0'));
        if !canonical {
            return Err(serde::de::Error::custom("amounts are plain decimal strings"));
        }
        text.parse().map(Wei).map_err(serde::de::Error::custom)
    }
}

/// A chain a swap can involve. Solana can be added when liq2sol joins (decision P6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Liquid,
    Polygon,
}

/// A direction: what the taker sends from, and receives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub from: Chain,
    pub to: Chain,
}

pub const L2P: Pair = Pair { from: Chain::Liquid, to: Chain::Polygon };
pub const P2L: Pair = Pair { from: Chain::Polygon, to: Chain::Liquid };

impl Pair {
    /// Every swap has Liquid on one side and another chain on the other.
    pub fn is_valid(&self) -> bool {
        self.from != self.to && (self.from == Chain::Liquid || self.to == Chain::Liquid)
    }

    /// Whether the taker pays on Liquid.
    pub fn taker_pays_liquid(&self) -> bool {
        self.from == Chain::Liquid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmountSide {
    Send,
    Receive,
}

/// One step of a maker's prices. A swap takes the first tier whose `up_to` covers what the taker sends, in USDT0 base units, and
/// that one rate prices the whole swap. It isn't marginal, so a quote has one exact rate to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTier {
    pub up_to: Units,
    /// The spread, in hundredths of a percent. 30 means the taker receives 99.7% of what it sends, a price of about 1.0030.
    pub bps: u32,
}

/// What the offer says about Liquid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiquidInfo {
    pub asset: String,
    pub confirmations: u32,
    pub fee_coin_sats: u64,
}

/// What the offer says about Polygon. A taker holds these to its own pinned values: an offer can't name a new escrow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolygonInfo {
    pub chain_id: u64,
    pub escrow: String,
    pub token: String,
    /// The most POL the maker's L→P lock forwards to the taker.
    pub max_gas_drop: Wei,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chains {
    pub liquid: LiquidInfo,
    pub polygon: PolygonInfo,
}

/// One direction a maker fills, with its limits and prices, all in USDT0 base units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairOffer {
    pub from: Chain,
    pub to: Chain,
    pub min: Units,
    pub max: Units,
    /// What the maker can pay right now in this direction. A maker may publish less than it holds, because this reveals its
    /// capacity. It doesn't quote above it.
    pub available: Units,
    pub prices: Vec<PriceTier>,
}

impl PairOffer {
    pub fn pair(&self) -> Pair {
        Pair { from: self.from, to: self.to }
    }
}

/// The public offer, in a kind 38401 event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub v: u32,
    pub network: String,
    pub template_version: u32,
    pub chains: Chains,
    pub pairs: Vec<PairOffer>,
    pub relays: Vec<String>,
}

impl Offer {
    pub fn pair(&self, pair: Pair) -> Option<&PairOffer> {
        self.pairs.iter().find(|p| p.pair() == pair)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub from: Chain,
    pub to: Chain,
    pub amount_side: AmountSide,
    pub amount: Units,
}

impl Request {
    pub fn pair(&self) -> Pair {
        Pair { from: self.from, to: self.to }
    }
}

/// `send` and `receive` are in USDT0 base units. Liquid-side amounts are those times 100, exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub send: Units,
    pub receive: Units,
    pub quote_expiry: u64,
    pub taker_lock_window: u32,
    /// L→P: the POL the maker's lock will forward to the taker, in wei (decision P2). Zero for P→L.
    pub gas_drop: Wei,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    /// The rumor id of the quote being accepted.
    pub quote_event: String,
    /// The swap's point T, compressed. The taker alone knows its secret.
    pub lock_point: String,
    /// The taker's swap key, compressed. The output's internal key aggregates this with the maker's.
    pub taker_key: String,
    /// The blinding key for the swap's coin, generated by the taker. It reads the amount, and can't spend anything.
    pub blinding_key: String,
    /// The taker's Polygon address: L→P, the recipient of the maker's lock; P→L, the initiator of the taker's own.
    pub taker_polygon: String,
    /// L→P with a gas drop: where it goes. Often the same as `taker_polygon`.
    pub gas_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MakerTerms {
    /// The rumor id of the accept this answers.
    pub accept_event: String,
    pub h_accept: u32,
    pub lock_height: u32,
    /// The maker's swap key, compressed.
    pub maker_key: String,
    /// The maker's Polygon address: L→P, the initiator of its lock; P→L, the recipient the taker locks to.
    pub maker_polygon: String,
    pub usdt_asset: String,
    pub chain_id: u64,
    pub escrow: String,
    pub token: String,
    pub template_version: u32,
    pub liquid_amount: Units,
    pub polygon_amount: Units,
    /// L→P: how long the maker's Polygon lock will run, on both clocks. P→L: the least time the taker's lock must leave on
    /// both clocks when the maker checks it (docs/design.md section 3.4).
    pub polygon_window_secs: u64,
    pub gas_drop: Wei,
}

/// L→P, maker to taker: the unsigned claim of the taker's coin, for the taker to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimOffer {
    pub lock_point: String,
    pub tx: String,
    pub nonce: String,
}

/// L→P, taker to maker: the taker's share of the adapter signature on that claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimAnswer {
    pub lock_point: String,
    pub nonce: String,
    pub share: String,
}

/// L→P, taker to maker: please never lock this swap. The refund itself comes later, as an `EarlyRefundAsk`, once the
/// taker has a fee coin and an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelAsk {
    pub lock_point: String,
}

/// L→P, maker to taker: it has promised never to lock this swap, and will sign the refund when asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancelled {
    pub lock_point: String,
}

/// L→P, taker to maker: please agree to this refund before you lock. Agreeing means never locking this swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarlyRefundAsk {
    pub lock_point: String,
    pub tx: String,
    pub nonce: String,
}

/// L→P, maker to taker: its share of the refund's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarlyRefundShare {
    pub lock_point: String,
    pub nonce: String,
    pub share: String,
}

/// P→L, taker to maker: please give my Polygon lock back, before you lock on Liquid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAsk {
    pub lock_point: String,
}

/// P→L, taker to maker: the unsigned claim of the maker's coin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpen {
    pub lock_point: String,
    pub tx: String,
    pub nonce: String,
}

/// P→L, maker to taker: the maker's nonce for that claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionNonce {
    pub lock_point: String,
    pub nonce: String,
}

/// P→L: a party's signature share. The taker sends first. The maker answers only after storing the adapter signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionShare {
    pub lock_point: String,
    pub share: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reject {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "body", rename_all = "kebab-case")]
pub enum Body {
    RfqRequest(Request),
    RfqQuote(Quote),
    RfqAccept(Accept),
    RfqTerms(MakerTerms),
    RfqReject(Reject),
    ClaimOffer(ClaimOffer),
    ClaimAnswer(ClaimAnswer),
    SessionOpen(SessionOpen),
    SessionNonce(SessionNonce),
    TakerShare(SessionShare),
    MakerShare(SessionShare),
    EarlyRefundAsk(EarlyRefundAsk),
    EarlyRefundShare(EarlyRefundShare),
    ReleaseAsk(ReleaseAsk),
    CancelAsk(CancelAsk),
    Cancelled(Cancelled),
}

/// Every private message. `id` is 32 random bytes in hex, set by the taker and repeated throughout a negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    pub network: String,
    pub sent_at: u64,
    #[serde(flatten)]
    pub body: Body,
}

/// Decode lowercase hex of an exact length. Other spellings are refused, so one value has one form.
pub fn hex32(s: &str) -> Option<[u8; 32]> {
    let bytes = hex_any(s)?;
    bytes.try_into().ok()
}

/// A compressed point or key: lowercase hex, 33 bytes, with a valid prefix. Whether it is on the curve is checked where it's used.
pub fn hex33(s: &str) -> Option<[u8; 33]> {
    let bytes: [u8; 33] = hex_any(s)?.try_into().ok()?;
    matches!(bytes[0], 2 | 3).then_some(bytes)
}

pub fn hex_any(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
