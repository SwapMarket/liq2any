//! The signer's own rules. These run in a separate process and trust nothing the engine says, so a compromised RFQ parser or
//! engine still can't get an out-of-policy signature. The signer keeps its own ledger. From liq2sol's, with Polygon in Solana's
//! place (docs/design.md section 4.2).

use serde::{Deserialize, Serialize};

use crate::engine::{blocks_for, secs_for, Config, Direction};
use crate::LockPoint;

/// What the signer's own chain connections show about the taker's Liquid coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoinEvidence {
    pub amount: u64,
    pub confirmations: u32,
}

/// What the signer's own chain connections show about the taker's Polygon lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowEvidence {
    pub amount: u64,
    /// Open, a long leg not yet extended, locked to this swap's `address(T)` with us as recipient.
    pub matches_terms: bool,
    /// Seconds left on the time clock and, at the measured rate, on the block clock.
    pub secs_left_time: u64,
    pub secs_left_blocks: u64,
}

/// Polygon's finalized block, as the signer's own connection reads it, and the measured block time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolygonClock {
    pub time: u64,
    pub block: u64,
    pub block_ms: u64,
}

/// A signing request, as the signer assembles it. Every field here comes from the signer itself: amounts and destinations
/// from transactions it builds, and heights, blocks and evidence from its own chain connections. A compromised engine
/// therefore can't lie its way past these rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// L→P: lock USDT0 as a short leg, forwarding a gas drop.
    LockPolygon {
        swap_id: [u8; 32],
        amount: u64,
        deadline_time: u64,
        deadline_block: u64,
        gas_drop: u128,
        clock: PolygonClock,
        liquid_lock_height: u32,
        liquid_tip: u32,
        taker_coin: Option<CoinEvidence>,
        /// Tether has blocked the taker's address, which a claim would still pay.
        recipient_blocked: bool,
    },
    /// L→P: agree to the taker's early refund, which is only safe if we have never signed a lock for this swap.
    EarlyRefund { swap_id: [u8; 32] },
    /// P→L: lock Liquid funds.
    LockLiquid {
        swap_id: [u8; 32],
        liquid_amount: u64,
        lock_height: u32,
        liquid_tip: u32,
        taker_escrow: Option<EscrowEvidence>,
        /// Tether has blocked our own Polygon address, so we couldn't move what we claim.
        maker_blocked: bool,
    },
    /// L→P: keep a checked adapter signature. Nothing leaves the signer, but a Polygon lock depends on it.
    HoldAdaptor { swap_id: [u8; 32] },
    /// P→L: keep the taker's adapter signature, and release our own share, which lets the taker finish the claim. Once only.
    GiveShare {
        swap_id: [u8; 32],
        lock_height: u32,
        liquid_tip: u32,
        /// The taker's lock is still there to collect. Once it's refunded, a claim here would be theft.
        taker_escrow_open: bool,
    },
    /// The claim pays our own wallet, because the signer builds it that way.
    ClaimLiquid { lock_point: LockPoint, secret: [u8; 32] },
    ClaimPolygon { lock_point: LockPoint, secret: [u8; 32] },
    /// The refund pays our own wallet, because the signer builds it that way.
    RefundLiquid { swap_id: [u8; 32], lock_height: u32, liquid_tip: u32 },
    RefundPolygon { swap_id: [u8; 32] },
    Extend {
        swap_id: [u8; 32],
        current_time: u64,
        current_block: u64,
        new_time: u64,
        new_block: u64,
        liquid_tip_age_secs: u64,
    },
    /// Give the taker's P→L lock back. Only ever before we locked anything for this swap.
    Release { swap_id: [u8; 32] },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    OverSwapLimit,
    OverWindowLimit,
    AlreadyLocked,
    UnknownSwap,
    WrongSecret,
    /// L→P: we'd have no way to collect the taker's coin after the taker claims.
    NoAdaptor,
    /// A swap has one signing session. Its result is kept and resent, never replaced.
    AdaptorAlreadyHeld,
    BadTimeout(String),
    /// Refused: we have claimed the taker's lock, so the Liquid coin is the taker's (liq2sol's protocol section 7.3).
    RefundAfterClaim,
    /// L→P: the taker's coin isn't there, or isn't confirmed enough. A compromised engine could otherwise lock our funds to a
    /// colluding taker who never paid.
    NoTakerCoin,
    /// P→L: the taker's lock isn't there, doesn't match, or is too close to its deadlines.
    NoTakerEscrow,
    /// We would pay out more than we are receiving.
    BadPrice,
    ExtendNotJustified,
    /// Refused: we locked on Liquid, so giving the taker its lock back would hand it both sides.
    ReleaseAfterLock,
    /// Refused: we agreed to refund the taker, who may already have the coin back, so we must never lock.
    EarlyRefunded,
    /// Tether has blocked an address the swap depends on.
    Blocked,
    /// The gas drop is over its cap.
    GasDropTooLarge,
    /// An EVM nonce already carries another call: it must never be replaced (docs/design.md section 4.2).
    NonceTaken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_per_swap: u64,
    pub window_secs: u64,
    pub max_per_window: u64,
    pub confirmations: u32,
    pub large_above: u64,
    pub confirmations_large: u32,
    pub p2l_gate_secs: u64,
    pub l2p_gate_blocks: u32,
    pub l2p_max_deadline_secs: u64,
    pub short_leg_block_slack_pct: u64,
    pub max_gas_drop: u128,
    pub p2l_min_blocks: u32,
    pub p2l_max_blocks: u32,
    pub extend_after_stall_secs: u64,
    pub extend_by_secs: u64,
    pub max_extension_blocks: u64,
    pub polygon_block_ms_min: u64,
    pub polygon_block_ms_max: u64,
}

impl From<&Config> for Limits {
    fn from(cfg: &Config) -> Self {
        Self {
            max_per_swap: cfg.max_per_swap,
            window_secs: 3_600,
            max_per_window: cfg.max_open_l2p.max(cfg.max_open_p2l),
            confirmations: cfg.confirmations,
            large_above: cfg.large_above,
            confirmations_large: cfg.confirmations_large,
            p2l_gate_secs: cfg.p2l_gate_secs,
            l2p_gate_blocks: cfg.l2p_gate_blocks,
            l2p_max_deadline_secs: cfg.l2p_deadline_secs,
            short_leg_block_slack_pct: cfg.short_leg_block_slack_pct,
            max_gas_drop: cfg.max_gas_drop,
            p2l_min_blocks: 30,
            p2l_max_blocks: 120,
            extend_after_stall_secs: cfg.extend_after_stall_secs,
            extend_by_secs: cfg.extend_by_secs,
            max_extension_blocks: cfg.max_extension_blocks,
            polygon_block_ms_min: cfg.polygon_block_ms_min,
            polygon_block_ms_max: cfg.polygon_block_ms_max,
        }
    }
}

/// The signer's memory of one swap. Persisted before any signature is released.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRecord {
    pub direction: Option<Direction>,
    /// In USDT0 base units.
    pub locked_amount: u64,
    /// L→P: a retry must have these exact deadlines and gas drop, or it could outlive the margin.
    pub deadline_time: u64,
    pub deadline_block: u64,
    pub gas_drop: u128,
    pub polygon_claimed: bool,
    pub liquid_refunded: bool,
    /// L→P: a checked adapter signature is stored. P→L: and our share of it has been given out.
    pub adaptor_held: bool,
    /// We gave the taker its money back early, so this swap is never locked: L→P by signing its refund, P→L by releasing its
    /// Polygon lock.
    pub early_refunded: bool,
}

/// What the check needs from the signer's ledger.
pub trait Ledger {
    fn record(&self, swap_id: &[u8; 32]) -> SwapRecord;
    /// Total locked in the current window, in USDT0 base units.
    fn locked_in_window(&self, now_secs: u64, window_secs: u64) -> u64;
}

fn id_of(request: &Request) -> [u8; 32] {
    use Request::*;
    match request {
        LockPolygon { swap_id, .. }
        | LockLiquid { swap_id, .. }
        | HoldAdaptor { swap_id }
        | EarlyRefund { swap_id }
        | GiveShare { swap_id, .. }
        | RefundLiquid { swap_id, .. }
        | RefundPolygon { swap_id }
        | Extend { swap_id, .. }
        | Release { swap_id } => *swap_id,
        ClaimLiquid { lock_point, .. } | ClaimPolygon { lock_point, .. } => lock_point.id(),
    }
}

fn clock_ok(limits: &Limits, clock: &PolygonClock) -> Result<(), Refusal> {
    if (limits.polygon_block_ms_min..=limits.polygon_block_ms_max).contains(&clock.block_ms) {
        Ok(())
    } else {
        Err(Refusal::BadTimeout(format!("a measured block time of {} ms", clock.block_ms)))
    }
}

/// Check one request. On `Ok`, the caller must persist the returned record, and only then release a signature.
pub fn check(limits: &Limits, ledger: &dyn Ledger, now_secs: u64, request: &Request) -> Result<SwapRecord, Refusal> {
    use Request::*;
    let mut record = ledger.record(&id_of(request));
    let check_lock = |amount: u64| -> Result<(), Refusal> {
        if record.direction.is_some() {
            return Err(Refusal::AlreadyLocked);
        }
        if amount > limits.max_per_swap {
            return Err(Refusal::OverSwapLimit);
        }
        let used = ledger.locked_in_window(now_secs, limits.window_secs);
        if used.checked_add(amount).is_none_or(|total| total > limits.max_per_window) {
            return Err(Refusal::OverWindowLimit);
        }
        Ok(())
    };
    match request {
        EarlyRefund { .. } => {
            // Any signed lock could still land, even one that seems lost, so one ever signed rules this out.
            if record.direction.is_some() {
                return Err(Refusal::AlreadyLocked);
            }
            record.early_refunded = true;
        }
        LockPolygon {
            amount,
            deadline_time,
            deadline_block,
            gas_drop,
            clock,
            liquid_lock_height,
            liquid_tip,
            taker_coin,
            recipient_blocked,
            ..
        } => {
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            if !record.adaptor_held {
                return Err(Refusal::NoAdaptor);
            }
            if *recipient_blocked {
                return Err(Refusal::Blocked);
            }
            if *gas_drop > limits.max_gas_drop {
                return Err(Refusal::GasDropTooLarge);
            }
            // A lost transaction may be signed again, if every term is identical.
            let retry = record.direction == Some(Direction::L2P)
                && record.locked_amount == *amount
                && record.deadline_time == *deadline_time
                && record.deadline_block == *deadline_block
                && record.gas_drop == *gas_drop;
            if !retry {
                check_lock(*amount)?;
            }
            let coin = taker_coin.ok_or(Refusal::NoTakerCoin)?;
            let needed = if *amount > limits.large_above { limits.confirmations_large } else { limits.confirmations };
            if coin.confirmations < needed {
                return Err(Refusal::NoTakerCoin);
            }
            if amount.checked_mul(100).is_none_or(|owed| coin.amount < owed) {
                return Err(Refusal::BadPrice);
            }
            if liquid_lock_height.saturating_sub(*liquid_tip) < limits.l2p_gate_blocks {
                return Err(Refusal::BadTimeout("too close to the taker's refund".into()));
            }
            clock_ok(limits, clock)?;
            let secs = deadline_time.saturating_sub(clock.time);
            if secs == 0 || secs > limits.l2p_max_deadline_secs {
                return Err(Refusal::BadTimeout(format!("deadline {secs} s away")));
            }
            // The block clock may run a little longer than the time clock, by the slack, and no more.
            let blocks = deadline_block.saturating_sub(clock.block);
            let most = blocks_for(limits.l2p_max_deadline_secs, clock.block_ms);
            let most = most.saturating_add(most.saturating_mul(limits.short_leg_block_slack_pct) / 100);
            if blocks == 0 || blocks > most {
                return Err(Refusal::BadTimeout(format!("deadline {blocks} blocks away")));
            }
            record.direction = Some(Direction::L2P);
            record.locked_amount = *amount;
            record.deadline_time = *deadline_time;
            record.deadline_block = *deadline_block;
            record.gas_drop = *gas_drop;
        }
        LockLiquid { liquid_amount, lock_height, liquid_tip, taker_escrow, maker_blocked, .. } => {
            // We gave the taker their Polygon lock back, so a Liquid lock now could only be lost.
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            if *maker_blocked {
                return Err(Refusal::Blocked);
            }
            let amount = liquid_amount.div_ceil(100);
            check_lock(amount)?;
            let escrow = taker_escrow.ok_or(Refusal::NoTakerEscrow)?;
            if !escrow.matches_terms
                || escrow.secs_left_time < limits.p2l_gate_secs
                || escrow.secs_left_blocks < limits.p2l_gate_secs
            {
                return Err(Refusal::NoTakerEscrow);
            }
            if amount > escrow.amount || liquid_amount % 100 != 0 {
                return Err(Refusal::BadPrice);
            }
            let blocks = lock_height.saturating_sub(*liquid_tip);
            if blocks < limits.p2l_min_blocks || blocks > limits.p2l_max_blocks {
                return Err(Refusal::BadTimeout(format!("lock height {blocks} blocks away")));
            }
            record.direction = Some(Direction::P2L);
            record.locked_amount = amount;
        }
        HoldAdaptor { .. } => {
            if record.early_refunded {
                return Err(Refusal::EarlyRefunded);
            }
            if record.direction.is_some() {
                return Err(Refusal::AlreadyLocked);
            }
            if record.adaptor_held {
                return Err(Refusal::AdaptorAlreadyHeld);
            }
            record.adaptor_held = true;
        }
        GiveShare { lock_height, liquid_tip, taker_escrow_open, .. } => {
            if record.direction != Some(Direction::P2L) {
                return Err(Refusal::UnknownSwap);
            }
            if record.adaptor_held {
                return Err(Refusal::AdaptorAlreadyHeld);
            }
            if record.liquid_refunded || liquid_tip >= lock_height {
                return Err(Refusal::BadTimeout("past the lock height".into()));
            }
            if !taker_escrow_open {
                return Err(Refusal::NoTakerEscrow);
            }
            record.adaptor_held = true;
        }
        ClaimLiquid { lock_point, secret } => {
            if record.direction != Some(Direction::L2P) {
                return Err(Refusal::UnknownSwap);
            }
            if !lock_point.opened_by(secret) {
                return Err(Refusal::WrongSecret);
            }
        }
        ClaimPolygon { lock_point, secret } => {
            if record.direction != Some(Direction::P2L) {
                return Err(Refusal::UnknownSwap);
            }
            if !lock_point.opened_by(secret) {
                return Err(Refusal::WrongSecret);
            }
            record.polygon_claimed = true;
        }
        RefundLiquid { lock_height, liquid_tip, .. } => {
            if record.direction != Some(Direction::P2L) {
                return Err(Refusal::UnknownSwap);
            }
            if record.polygon_claimed {
                return Err(Refusal::RefundAfterClaim);
            }
            if liquid_tip < lock_height {
                return Err(Refusal::BadTimeout("before the lock height".into()));
            }
            record.liquid_refunded = true;
        }
        RefundPolygon { .. } => {
            if record.direction != Some(Direction::L2P) {
                return Err(Refusal::UnknownSwap);
            }
        }
        Release { .. } => {
            if record.direction.is_some() {
                return Err(Refusal::ReleaseAfterLock);
            }
            // And never lock on Liquid afterwards.
            record.early_refunded = true;
        }
        Extend { current_time, current_block, new_time, new_block, liquid_tip_age_secs, .. } => {
            if record.direction != Some(Direction::P2L) {
                return Err(Refusal::UnknownSwap);
            }
            if *liquid_tip_age_secs < limits.extend_after_stall_secs {
                return Err(Refusal::ExtendNotJustified);
            }
            if *new_time <= *current_time || new_time - current_time > limits.extend_by_secs {
                return Err(Refusal::BadTimeout("time extension out of range".into()));
            }
            if *new_block <= *current_block || new_block - current_block > limits.max_extension_blocks {
                return Err(Refusal::BadTimeout("block extension out of range".into()));
            }
        }
    }
    Ok(record)
}

/// The amount a request newly locks, for the window count. A retry of an existing lock adds nothing.
pub fn newly_locked(before: &SwapRecord, request: &Request) -> Option<u64> {
    if before.direction.is_some() {
        return None;
    }
    match request {
        Request::LockPolygon { amount, .. } => Some(*amount),
        Request::LockLiquid { liquid_amount, .. } => Some(liquid_amount.div_ceil(100)),
        _ => None,
    }
}

/// The EVM nonce rule (docs/design.md section 4.2). `taken` is the call already recorded at this nonce, by the hash of its
/// destination, value and data, or None. The same call may be signed again at a new fee, to unstick it; a different call
/// never, since replacing a pending transaction could drop a lock, a claim or a refund that something else relies on.
pub fn nonce_rule(taken: Option<&[u8; 32]>, call: &[u8; 32]) -> Result<(), Refusal> {
    match taken {
        Some(existing) if existing != call => Err(Refusal::NonceTaken),
        _ => Ok(()),
    }
}

/// Seconds left on an escrow's two clocks, for `EscrowEvidence`.
pub fn secs_left(deadline_time: u64, deadline_block: u64, clock: &PolygonClock) -> (u64, u64) {
    (
        deadline_time.saturating_sub(clock.time),
        secs_for(deadline_block.saturating_sub(clock.block), clock.block_ms),
    )
}
