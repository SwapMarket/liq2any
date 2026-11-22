//! One swap's state machine. `step` is a pure function: (state, what the chains show) -> (new state, actions).
//! Specification: docs/design.md sections 2.4 and 3, built on liq2sol's (docs/copied.md).
//!
//! Actions may be repeated if the daemon crashes or a broadcast is lost, so each must be safe to repeat. The one thing that
//! isn't is paying twice to the Liquid swap address, which is why `LockLiquid` is emitted once and retries use
//! `RebroadcastLock`, the identical stored transaction. On Polygon, the signer keeps each call at one nonce, so a repeated
//! `LockPolygon` can't lock twice either (docs/design.md section 4.2).

use serde::{Deserialize, Serialize};

use liq2any_swapkey::secret_from_polygon;

use crate::LockPoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Taker locks L-USDt, maker locks USDT0 on Polygon.
    L2P,
    /// Taker locks USDT0 on Polygon, maker locks L-USDt.
    P2L,
}

/// Limits from docs/design.md sections 3.4 and 4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub confirmations: u32,
    /// Swaps above this many USDT0 base units need `confirmations_large` (liq2sol's decision D7).
    pub large_above: u64,
    pub confirmations_large: u32,
    pub max_per_swap: u64,
    pub max_open_l2p: u64,
    pub max_open_p2l: u64,
    /// L→P: lock only with at least this many blocks before `lock_height`.
    pub l2p_gate_blocks: u32,
    pub l2p_fund_window_blocks: u32,
    /// Length of the short Polygon lock the maker creates, in seconds. Its block deadline is the same time at the measured
    /// rate, plus `short_leg_block_slack_pct`, so the block clock only decides when the time clock runs slow.
    pub l2p_deadline_secs: u64,
    pub short_leg_block_slack_pct: u64,
    /// L→P: the most POL a swap's gas drop may be, in wei.
    pub max_gas_drop: u128,
    /// P→L: lock only with at least this much time left on both of the taker's clocks.
    pub p2l_gate_secs: u64,
    pub p2l_lock_window_blocks: u32,
    pub liquid_stale_secs: u64,
    /// Polygon's finalized block must have moved within this time...
    pub polygon_stale_secs: u64,
    /// ...and trail the head by no more than this, or a milestone stall is under way (docs/design.md section 4.4).
    pub polygon_max_lag_blocks: u64,
    /// A measured block time outside this range means the measurement, or the chain, is wrong.
    pub polygon_block_ms_min: u64,
    pub polygon_block_ms_max: u64,
    /// P→L: extend when Liquid has been stalled this long...
    pub extend_after_stall_secs: u64,
    /// ...and the taker's refund is this close...
    pub extend_within_secs: u64,
    /// ...by this much, and in blocks by the same time at the measured rate, up to the escrow's own limit.
    pub extend_by_secs: u64,
    pub max_extension_blocks: u64,
    /// L→P: page when a reveal is unclaimed and the lock height is this close.
    pub urgent_blocks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            confirmations: 2,
            large_above: 1_000_000_000,
            confirmations_large: 10,
            max_per_swap: 1_000_000_000,
            max_open_l2p: 5_000_000_000,
            max_open_p2l: 5_000_000_000,
            l2p_gate_blocks: 390,
            l2p_fund_window_blocks: 30,
            l2p_deadline_secs: 3 * 3_600,
            short_leg_block_slack_pct: 10,
            max_gas_drop: 500_000_000_000_000_000,
            p2l_gate_secs: 6 * 3_600 + 1_800,
            p2l_lock_window_blocks: 30,
            liquid_stale_secs: 600,
            polygon_stale_secs: 60,
            polygon_max_lag_blocks: 100,
            polygon_block_ms_min: 500,
            polygon_block_ms_max: 5_000,
            extend_after_stall_secs: 3_600,
            extend_within_secs: 3_600,
            extend_by_secs: 7 * 86_400,
            max_extension_blocks: 403_200,
            urgent_blocks: 60,
        }
    }
}

/// What both sides agreed in `rfq-terms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terms {
    pub direction: Direction,
    pub lock_point: LockPoint,
    pub liquid_amount: u64,
    /// USDT0 base units, 6 decimals.
    pub polygon_amount: u64,
    pub h_accept: u32,
    pub lock_height: u32,
    /// L→P: POL in wei the maker's lock forwards to the taker (decision P2). Zero for P→L.
    pub gas_drop: u128,
}

impl Terms {
    pub fn swap_id(&self) -> [u8; 32] {
        self.lock_point.id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Waiting for the taker's lock.
    WatchLock,
    /// L→P: our Polygon lock is sent, with both deadlines fixed for every retry.
    LockedPolygon { deadline_time: u64, deadline_block: u64 },
    /// P→L: our Liquid lock is sent.
    LockedLiquid,
    /// The secret is known, and our claim on the taker's chain is in progress.
    Claiming { secret: [u8; 32] },
    /// Our refund is in progress.
    Refunding,
    /// Nothing more to do: claimed, refunded or lost, as `outcome` says.
    Done { outcome: Outcome },
    /// The swap never started, so no funds of ours moved.
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Completed,
    Refunded,
    /// Our funds are gone without payment. Always paged.
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EscrowState {
    Open,
    Claimed,
    Refunded,
}

/// The swap's Polygon lock, read at `finalized`. An EVM lock never disappears once made, unlike liq2sol's Solana escrow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscrowView {
    pub state: EscrowState,
    pub amount: u64,
    /// It locks to `address(T)` for this swap's point.
    pub lock_address_ok: bool,
    /// The recipient is the party that should receive this lock under the terms.
    pub recipient_ok: bool,
    pub deadline_time: u64,
    pub deadline_block: u64,
    pub long_leg: bool,
    pub extended: bool,
    /// What the claim revealed, `s = t * Gx mod n`.
    pub revealed: [u8; 32],
}

impl EscrowView {
    /// The escrow's own clock rule (docs/design.md section 2.4).
    pub fn can_claim(&self, time: u64, block: u64) -> bool {
        let time_open = time < self.deadline_time;
        let blocks_open = block < self.deadline_block;
        self.state == EscrowState::Open && if self.long_leg { time_open || blocks_open } else { time_open && blocks_open }
    }
}

/// What is known about the swap's Liquid coin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HtlcView {
    /// No coin that unblinds to exactly the right asset and amount. The chain doesn't check these, only we do.
    Absent,
    Unspent {
        confirmations: u32,
    },
    /// Spent by a key-path claim, whose signature gave this secret, which is checked against the point here. A secret is
    /// usable at once, but a swap isn't finished until the spend is `confirmed`, because an unconfirmed one can still drop.
    SpentByClaim {
        secret: [u8; 32],
        confirmed: bool,
    },
    /// Spent by the refund path.
    SpentByRefund {
        confirmed: bool,
    },
}

/// One tick's view of the world. The caller only sets values that its sources agree on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub liquid_tip: u32,
    pub liquid_tip_age_secs: u64,
    /// Polygon's finalized block: its timestamp and number.
    pub polygon_time: u64,
    pub polygon_block: u64,
    /// How long since the finalized block last moved, by our own clock.
    pub polygon_age_secs: u64,
    /// The head's number minus the finalized one.
    pub polygon_lag_blocks: u64,
    /// The measured time per block, in milliseconds.
    pub polygon_block_ms: u64,
    /// Liquid's tip went down, or a confirmed transaction vanished. Latched until an operator clears it.
    pub rollback_detected: bool,
    /// The sources of either chain disagree, so nothing they say can start a new lock.
    pub sources_disagree: bool,
    /// Tether hasn't blocked our Polygon address. P→L: we must be able to move what we claim.
    pub maker_unblocked: bool,
    /// Tether hasn't blocked the taker's Polygon address. L→P: a claim would still pay it, and the coins would be stuck.
    pub taker_unblocked: bool,
    /// L→P: the signer holds a checked adapter signature for the claim, so a Polygon lock can't lose our funds.
    pub adaptor_held: bool,
    pub htlc: HtlcView,
    pub escrow: Option<EscrowView>,
    /// Open exposure in this direction, excluding this swap.
    pub open_exposure: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// L→P: the coin is good, so run a signing session with the taker. Safe to repeat.
    RequestAdaptor,
    /// L→P: lock the maker's USDT0 as a short leg, forwarding the gas drop.
    LockPolygon { deadline_time: u64, deadline_block: u64, gas_drop: u128 },
    /// P→L: pay the swap address and the taker's fee coin. Emitted at most once per swap.
    LockLiquid,
    /// P→L: send the identical stored lock transaction again.
    RebroadcastLock,
    /// L→P: claim the taker's coin on Liquid.
    ClaimLiquid { secret: [u8; 32] },
    /// P→L: claim the taker's lock on Polygon.
    ClaimPolygon { secret: [u8; 32] },
    /// L→P: take back our Polygon lock.
    RefundPolygon,
    /// P→L: take back our Liquid lock.
    RefundLiquid,
    /// P→L: move the taker's refund later during a Liquid halt, on both clocks.
    Extend { new_time: u64, new_block: u64 },
    /// P→L: we won't fill this swap, so give the taker's funds back at once.
    Release,
    Page(Page),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Rollback,
    /// A known secret still isn't claimed, and time is short: in Liquid blocks for L→P, in seconds for P→L.
    ClaimUrgent { left: u64 },
    /// Our Liquid lock never appeared by the lock height.
    LockMissing,
    Lost,
    /// An on-chain value that should be impossible, such as a revealed value that doesn't open the point.
    Impossible(String),
    /// A wallet is too low to fill the largest trade, or to pay for a few more swaps.
    LowBalance(String),
}

/// `secs` of Polygon time in blocks, at `block_ms` per block, rounded up.
pub fn blocks_for(secs: u64, block_ms: u64) -> u64 {
    secs.saturating_mul(1_000).div_ceil(block_ms.max(1))
}

/// `blocks` of Polygon in seconds, at `block_ms` per block, rounded down.
pub fn secs_for(blocks: u64, block_ms: u64) -> u64 {
    blocks.saturating_mul(block_ms) / 1_000
}

fn required_confirmations(cfg: &Config, terms: &Terms) -> u32 {
    if terms.polygon_amount > cfg.large_above {
        cfg.confirmations_large
    } else {
        cfg.confirmations
    }
}

/// Whether both chains are fresh, final enough and agree, so new risk can be taken on.
fn healthy(cfg: &Config, v: &View) -> bool {
    v.liquid_tip_age_secs <= cfg.liquid_stale_secs
        && v.polygon_age_secs <= cfg.polygon_stale_secs
        && v.polygon_lag_blocks <= cfg.polygon_max_lag_blocks
        && (cfg.polygon_block_ms_min..=cfg.polygon_block_ms_max).contains(&v.polygon_block_ms)
        && !v.sources_disagree
}

fn within_limits(cfg: &Config, terms: &Terms, v: &View) -> bool {
    let max_open = match terms.direction {
        Direction::L2P => cfg.max_open_l2p,
        Direction::P2L => cfg.max_open_p2l,
    };
    terms.polygon_amount <= cfg.max_per_swap
        && v.open_exposure.checked_add(terms.polygon_amount).is_some_and(|total| total <= max_open)
}

/// The L→P gate: docs/design.md section 3.4, and liq2sol's maker.md section 3.
pub fn l2p_may_lock(cfg: &Config, terms: &Terms, v: &View) -> bool {
    l2p_coin_acceptable(cfg, terms, v) && v.adaptor_held
}

/// Everything the L→P gate needs except the adapter signature, which is worth asking for only once this holds.
fn l2p_coin_acceptable(cfg: &Config, terms: &Terms, v: &View) -> bool {
    let coin_ok =
        matches!(v.htlc, HtlcView::Unspent { confirmations } if confirmations >= required_confirmations(cfg, terms));
    coin_ok
        && terms.lock_height.saturating_sub(v.liquid_tip) >= cfg.l2p_gate_blocks
        && v.liquid_tip <= terms.h_accept.saturating_add(cfg.l2p_fund_window_blocks)
        && v.escrow.is_none()
        && v.taker_unblocked
        && terms.gas_drop <= cfg.max_gas_drop
        && healthy(cfg, v)
        && within_limits(cfg, terms, v)
}

/// The deadlines of a short lock made now.
pub fn l2p_deadlines(cfg: &Config, v: &View) -> (u64, u64) {
    let time = v.polygon_time.saturating_add(cfg.l2p_deadline_secs);
    let blocks = blocks_for(cfg.l2p_deadline_secs, v.polygon_block_ms);
    let slack = blocks.saturating_mul(cfg.short_leg_block_slack_pct) / 100;
    (time, v.polygon_block.saturating_add(blocks).saturating_add(slack))
}

/// Time left on each of an escrow's clocks, in seconds.
fn time_left(e: &EscrowView, v: &View) -> (u64, u64) {
    let by_time = e.deadline_time.saturating_sub(v.polygon_time);
    let by_blocks = secs_for(e.deadline_block.saturating_sub(v.polygon_block), v.polygon_block_ms);
    (by_time, by_blocks)
}

/// The taker's P→L lock must match the terms exactly, and leave the gate's time on both clocks: whichever clock a halt or a
/// change in block time spoils, the other still gives us our margin.
pub fn p2l_escrow_acceptable(cfg: &Config, terms: &Terms, v: &View) -> bool {
    v.escrow.as_ref().is_some_and(|e| {
        let (by_time, by_blocks) = time_left(e, v);
        e.state == EscrowState::Open
            && e.amount == terms.polygon_amount
            && e.lock_address_ok
            && e.recipient_ok
            && e.long_leg
            && !e.extended
            && by_time >= cfg.p2l_gate_secs
            && by_blocks >= cfg.p2l_gate_secs
    })
}

/// The P→L gate.
pub fn p2l_may_lock(cfg: &Config, terms: &Terms, v: &View) -> bool {
    p2l_escrow_acceptable(cfg, terms, v)
        && v.htlc == HtlcView::Absent
        && v.liquid_tip <= terms.h_accept.saturating_add(cfg.p2l_lock_window_blocks)
        && v.maker_unblocked
        && healthy(cfg, v)
        && within_limits(cfg, terms, v)
}

/// Decide the next state and actions for one swap.
pub fn step(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    // A rollback means nothing we believed is reliable. Stop everything and wake a human.
    if v.rollback_detected {
        let actions = match state {
            State::Done { .. } | State::Abandoned => vec![],
            _ => vec![Action::Page(Page::Rollback)],
        };
        return (state.clone(), actions);
    }
    match terms.direction {
        Direction::L2P => step_l2p(cfg, terms, state, v),
        Direction::P2L => step_p2l(cfg, terms, state, v),
    }
}

fn step_l2p(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match state {
        State::WatchLock => {
            if l2p_may_lock(cfg, terms, v) {
                let (deadline_time, deadline_block) = l2p_deadlines(cfg, v);
                (
                    State::LockedPolygon { deadline_time, deadline_block },
                    vec![Action::LockPolygon { deadline_time, deadline_block, gas_drop: terms.gas_drop }],
                )
            } else if l2p_coin_acceptable(cfg, terms, v) {
                (state.clone(), vec![Action::RequestAdaptor])
            } else if v.liquid_tip > terms.h_accept.saturating_add(cfg.l2p_fund_window_blocks) {
                // Nothing of ours moved. A coin the taker sent late is theirs to refund.
                (State::Abandoned, vec![])
            } else {
                (state.clone(), vec![])
            }
        }
        State::LockedPolygon { .. } => l2p_locked(cfg, terms, state, v),
        State::LockedLiquid => wrong_direction(state),
        State::Claiming { secret } => match &v.htlc {
            HtlcView::SpentByClaim { confirmed: true, .. } => (State::Done { outcome: Outcome::Completed }, vec![]),
            // Ours is in the mempool. Sending it again is harmless, and it survives a dropped transaction.
            HtlcView::SpentByClaim { confirmed: false, .. } => {
                (state.clone(), vec![Action::ClaimLiquid { secret: *secret }])
            }
            HtlcView::SpentByRefund { confirmed: true } => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            _ => {
                let mut actions = vec![Action::ClaimLiquid { secret: *secret }];
                let left = terms.lock_height.saturating_sub(v.liquid_tip);
                if left <= cfg.urgent_blocks {
                    actions.push(Action::Page(Page::ClaimUrgent { left: u64::from(left) }));
                }
                (state.clone(), actions)
            }
        },
        State::Refunding => match v.escrow.as_ref().map(|e| e.state) {
            Some(EscrowState::Refunded) => (State::Done { outcome: Outcome::Refunded }, vec![]),
            // A claim that landed first still pays us on Liquid, so go back and take it.
            Some(EscrowState::Claimed) => l2p_locked(cfg, terms, state, v),
            _ => (state.clone(), vec![Action::RefundPolygon]),
        },
        State::Done { .. } | State::Abandoned => (state.clone(), vec![]),
    }
}

/// A stored state from the other direction means the store is corrupt, so do nothing and wake a human.
fn wrong_direction(state: &State) -> (State, Vec<Action>) {
    (state.clone(), vec![Action::Page(Page::Impossible("state does not match the swap's direction".into()))])
}

fn l2p_locked(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    let Some(escrow) = &v.escrow else {
        let State::LockedPolygon { deadline_time, deadline_block } = state else {
            return (state.clone(), vec![]);
        };
        // The escrow refuses a lock once either deadline has passed, and the finalized block is past it, so the lock never
        // landed and now can't. No funds of ours moved.
        if v.polygon_time >= *deadline_time || v.polygon_block >= *deadline_block {
            return (State::Abandoned, vec![]);
        }
        // Repeating is safe: the signer resends the same call at the same nonce, and the escrow's id can be used once. It
        // keeps the same deadlines, and stops once a late landing would no longer leave the margin the gate promises.
        let safe = terms.lock_height.saturating_sub(v.liquid_tip) >= cfg.l2p_gate_blocks;
        let actions = if safe {
            vec![Action::LockPolygon {
                deadline_time: *deadline_time,
                deadline_block: *deadline_block,
                gas_drop: terms.gas_drop,
            }]
        } else {
            vec![]
        };
        return (state.clone(), actions);
    };
    match escrow.state {
        EscrowState::Claimed => {
            // The escrow checked this, so a failure here means the store or the RPC is lying.
            let Some(secret) = secret_from_polygon(&escrow.revealed).ok().filter(|t| terms.lock_point.opened_by(t))
            else {
                let page = Page::Impossible("the revealed value does not open the lock point".into());
                return (state.clone(), vec![Action::Page(page)]);
            };
            step_l2p(cfg, terms, &State::Claiming { secret }, v)
        }
        EscrowState::Refunded => (State::Done { outcome: Outcome::Refunded }, vec![]),
        EscrowState::Open => {
            if escrow.can_claim(v.polygon_time, v.polygon_block) {
                (state.clone(), vec![])
            } else {
                (State::Refunding, vec![Action::RefundPolygon])
            }
        }
    }
}

fn step_p2l(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match state {
        State::WatchLock => {
            if p2l_may_lock(cfg, terms, v) {
                (State::LockedLiquid, vec![Action::LockLiquid])
            } else if v.liquid_tip > terms.h_accept.saturating_add(cfg.p2l_lock_window_blocks) {
                // We won't fill this. If the taker locked, hand its funds back rather than make it wait.
                let open = v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Open && e.recipient_ok);
                (State::Abandoned, if open { vec![Action::Release] } else { vec![] })
            } else {
                (state.clone(), vec![])
            }
        }
        State::LockedLiquid => p2l_locked(cfg, terms, state, v),
        State::LockedPolygon { .. } => wrong_direction(state),
        State::Claiming { secret } => match v.escrow.as_ref() {
            Some(e) if e.state == EscrowState::Claimed => (State::Done { outcome: Outcome::Completed }, vec![]),
            Some(e) if e.state == EscrowState::Refunded => {
                (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
            }
            escrow => {
                let mut actions = vec![Action::ClaimPolygon { secret: *secret }];
                if let Some(e) = escrow {
                    // A long leg's claim stays open while either clock does.
                    let (by_time, by_blocks) = time_left(e, v);
                    let left = by_time.max(by_blocks);
                    if left <= cfg.extend_within_secs {
                        actions.push(Action::Page(Page::ClaimUrgent { left }));
                    }
                }
                (state.clone(), actions)
            }
        },
        State::Refunding => match &v.htlc {
            HtlcView::SpentByRefund { confirmed: true } => (State::Done { outcome: Outcome::Refunded }, vec![]),
            HtlcView::SpentByRefund { confirmed: false } => (state.clone(), p2l_refund_actions(v)),
            // The taker's late claim won the race. It pays us on Polygon.
            HtlcView::SpentByClaim { .. } => p2l_spent_by_claim(cfg, terms, state, v),
            _ => (state.clone(), p2l_refund_actions(v)),
        },
        State::Done { .. } | State::Abandoned => (state.clone(), vec![]),
    }
}

/// A coin whose Polygon lock we have already claimed belongs to the taker, even past the lock height.
fn p2l_refund_actions(v: &View) -> Vec<Action> {
    if v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed) {
        vec![]
    } else {
        vec![Action::RefundLiquid]
    }
}

fn p2l_spent_by_claim(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    let HtlcView::SpentByClaim { secret, .. } = &v.htlc else {
        return (state.clone(), vec![]);
    };
    // Both keys signed this spend, and we only signed the session we kept. Anything else needs a human.
    if !terms.lock_point.opened_by(secret) {
        let page = Page::Impossible("the claim's signature does not give the secret".into());
        return (state.clone(), vec![Action::Page(page)]);
    }
    step_p2l(cfg, terms, &State::Claiming { secret: *secret }, v)
}

fn p2l_locked(cfg: &Config, terms: &Terms, state: &State, v: &View) -> (State, Vec<Action>) {
    match &v.htlc {
        HtlcView::SpentByClaim { .. } => p2l_spent_by_claim(cfg, terms, state, v),
        HtlcView::SpentByRefund { confirmed: true } => (State::Done { outcome: Outcome::Refunded }, vec![]),
        HtlcView::SpentByRefund { confirmed: false } => (State::Refunding, vec![]),
        // Our lock isn't visible. Only ever resend the same transaction, so we can't pay twice.
        HtlcView::Absent if v.liquid_tip < terms.lock_height => (state.clone(), vec![Action::RebroadcastLock]),
        HtlcView::Absent => (state.clone(), vec![Action::Page(Page::LockMissing)]),
        HtlcView::Unspent { .. } => {
            if v.liquid_tip >= terms.lock_height {
                let actions = p2l_refund_actions(v);
                if actions.is_empty() {
                    return (state.clone(), actions);
                }
                return (State::Refunding, actions);
            }
            (state.clone(), p2l_extend_if_stalled(cfg, v))
        }
    }
}

/// liq2sol's decision D1, on both clocks: during a long Liquid stall, keep the taker's Polygon refund closed, or a claim after
/// the stall steals.
fn p2l_extend_if_stalled(cfg: &Config, v: &View) -> Vec<Action> {
    let Some(e) = &v.escrow else { return vec![] };
    let stalled = v.liquid_tip_age_secs >= cfg.extend_after_stall_secs;
    let (by_time, by_blocks) = time_left(e, v);
    let near = by_time.max(by_blocks) <= cfg.extend_within_secs;
    if e.state == EscrowState::Open
        && e.long_leg
        && stalled
        && near
        && !e.extended
        && e.can_claim(v.polygon_time, v.polygon_block)
    {
        let blocks = blocks_for(cfg.extend_by_secs, v.polygon_block_ms).min(cfg.max_extension_blocks);
        vec![Action::Extend {
            new_time: e.deadline_time.saturating_add(cfg.extend_by_secs),
            new_block: e.deadline_block.saturating_add(blocks),
        }]
    } else {
        vec![]
    }
}
