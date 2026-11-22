//! Every rule in docs/design.md sections 2.4 and 3, on liq2sol's protocol.md sections 4 and 5 and maker.md sections 3 and 4.

use liq2any_maker_core::engine::*;
use liq2any_maker_core::LockPoint;
use liq2any_swapkey::{point_of, polygon_value};
use proptest::prelude::*;

const P: [u8; 32] = [7; 32];
const USDT: u64 = 100_000_000;
const ACCEPT: u32 = 1_000;
const NOW: u64 = 1_800_000_000;
const BLOCK: u64 = 50_000_000;
const MS: u64 = 1_500;

fn point() -> LockPoint {
    LockPoint(point_of(&P).unwrap())
}

fn cfg() -> Config {
    Config::default()
}

fn terms(direction: Direction) -> Terms {
    let (lock_height, gas_drop) = match direction {
        Direction::L2P => (ACCEPT + 480, 100_000_000_000_000_000),
        Direction::P2L => (ACCEPT + 90, 0),
    };
    Terms {
        direction,
        lock_point: point(),
        liquid_amount: USDT * 100,
        polygon_amount: USDT,
        h_accept: ACCEPT,
        lock_height,
        gas_drop,
    }
}

fn view() -> View {
    View {
        liquid_tip: ACCEPT + 5,
        liquid_tip_age_secs: 30,
        polygon_time: NOW,
        polygon_block: BLOCK,
        polygon_age_secs: 2,
        polygon_lag_blocks: 3,
        polygon_block_ms: MS,
        rollback_detected: false,
        sources_disagree: false,
        maker_unblocked: true,
        taker_unblocked: true,
        adaptor_held: true,
        htlc: HtlcView::Absent,
        escrow: None,
        open_exposure: 0,
    }
}

/// The taker's P→L lock: 8 hours on both clocks.
fn taker_escrow() -> EscrowView {
    EscrowView {
        state: EscrowState::Open,
        amount: USDT,
        lock_address_ok: true,
        recipient_ok: true,
        deadline_time: NOW + 8 * 3_600,
        deadline_block: BLOCK + 19_200,
        long_leg: true,
        extended: false,
        revealed: [0; 32],
    }
}

/// Our L→P lock, as made by `l2p_deadlines` from `view()`.
fn maker_escrow() -> EscrowView {
    let (deadline_time, deadline_block) = l2p_deadlines(&cfg(), &view());
    EscrowView { deadline_time, deadline_block, long_leg: false, ..taker_escrow() }
}

fn run(t: &Terms, s: &State, v: &View) -> (State, Vec<Action>) {
    step(&cfg(), t, s, v)
}

fn coin(confirmations: u32) -> HtlcView {
    HtlcView::Unspent { confirmations }
}

// ---- Deadlines ----

#[test]
fn a_short_leg_gets_three_hours_and_its_block_clock_ten_percent_slack() {
    let (time, block) = l2p_deadlines(&cfg(), &view());
    assert_eq!(time, NOW + 10_800);
    // 10,800 s at 1.5 s a block is 7,200 blocks, plus 720.
    assert_eq!(block, BLOCK + 7_920);
    // At Amoy's 1 s a block: 10,800 plus 1,080.
    let (_, amoy) = l2p_deadlines(&cfg(), &View { polygon_block_ms: 1_000, ..view() });
    assert_eq!(amoy, BLOCK + 11_880);
}

#[test]
fn the_escrow_clock_rule_is_the_contracts() {
    let short = maker_escrow();
    let long = taker_escrow();
    let (t, b) = (short.deadline_time, short.deadline_block);
    assert!(short.can_claim(t - 1, b - 1));
    assert!(!short.can_claim(t, b - 1), "the time clock alone closes a short leg");
    assert!(!short.can_claim(t - 1, b), "and so does the block clock");
    let (t, b) = (long.deadline_time, long.deadline_block);
    assert!(long.can_claim(t, b - 1), "a long leg stays open on blocks");
    assert!(long.can_claim(t - 1, b), "or on time");
    assert!(!long.can_claim(t, b));
    assert!(!EscrowView { state: EscrowState::Claimed, ..long }.can_claim(0, 0));
}

// ---- L to P ----

#[test]
fn l2p_locks_once_the_coin_is_confirmed_with_the_gas_drop() {
    let t = terms(Direction::L2P);
    let (s, a) = run(&t, &State::WatchLock, &View { htlc: coin(2), ..view() });
    let (deadline_time, deadline_block) = l2p_deadlines(&cfg(), &view());
    assert_eq!(s, State::LockedPolygon { deadline_time, deadline_block });
    assert_eq!(a, vec![Action::LockPolygon { deadline_time, deadline_block, gas_drop: t.gas_drop }]);
}

#[test]
fn l2p_asks_for_the_adapter_signature_and_never_locks_without_it() {
    let t = terms(Direction::L2P);
    let good = View { htlc: coin(2), adaptor_held: false, ..view() };
    let (s, a) = run(&t, &State::WatchLock, &good);
    assert_eq!((s, a), (State::WatchLock, vec![Action::RequestAdaptor]));
    assert!(!l2p_may_lock(&cfg(), &t, &good));
    let (s, a) = run(&t, &State::WatchLock, &View { liquid_tip: ACCEPT + 31, ..good });
    assert_eq!((s, a), (State::Abandoned, vec![]));
}

#[test]
fn l2p_will_not_lock_without_each_condition() {
    let t = terms(Direction::L2P);
    let good = View { htlc: coin(2), ..view() };
    assert!(l2p_may_lock(&cfg(), &t, &good));
    let bad = [
        View { htlc: HtlcView::Absent, ..good.clone() },
        View { htlc: coin(1), ..good.clone() },
        View { liquid_tip: ACCEPT + 31, ..good.clone() },
        View { liquid_tip_age_secs: 601, ..good.clone() },
        View { polygon_age_secs: 61, ..good.clone() },
        View { polygon_lag_blocks: 101, ..good.clone() }, // A milestone stall: blocks go on, finality doesn't.
        View { polygon_block_ms: 499, ..good.clone() },
        View { polygon_block_ms: 5_001, ..good.clone() },
        View { sources_disagree: true, ..good.clone() },
        View { taker_unblocked: false, ..good.clone() },
        View { open_exposure: cfg().max_open_l2p, ..good.clone() },
        View { escrow: Some(maker_escrow()), ..good.clone() },
    ];
    for v in bad {
        let (_, a) = run(&t, &State::WatchLock, &v);
        assert!(a.is_empty(), "{v:?} gave {a:?}");
    }
    let greedy = Terms { gas_drop: cfg().max_gas_drop + 1, ..t };
    assert!(!l2p_may_lock(&cfg(), &greedy, &good), "a gas drop over the cap");
}

#[test]
fn l2p_gate_margin_is_exact() {
    let mut t = terms(Direction::L2P);
    t.lock_height = ACCEPT + 5 + 390;
    let v = View { htlc: coin(2), ..view() };
    assert!(l2p_may_lock(&cfg(), &t, &v));
    t.lock_height -= 1;
    assert!(!l2p_may_lock(&cfg(), &t, &v), "389 blocks of margin must refuse");
}

#[test]
fn large_swaps_need_more_confirmations_and_over_limit_swaps_never_lock() {
    let c = Config { max_per_swap: u64::MAX, max_open_l2p: u64::MAX, ..cfg() };
    let t = Terms { polygon_amount: c.large_above + 1, ..terms(Direction::L2P) };
    assert!(!l2p_may_lock(&c, &t, &View { htlc: coin(9), ..view() }));
    assert!(l2p_may_lock(&c, &t, &View { htlc: coin(10), ..view() }));
    let t = Terms { polygon_amount: cfg().max_per_swap + 1, ..terms(Direction::L2P) };
    assert!(!l2p_may_lock(&cfg(), &t, &View { htlc: coin(99), ..view() }));
}

#[test]
fn l2p_retries_a_lost_lock_with_the_same_deadlines_and_gives_up_when_either_passes() {
    let t = terms(Direction::L2P);
    let (dt, db) = l2p_deadlines(&cfg(), &view());
    let s = State::LockedPolygon { deadline_time: dt, deadline_block: db };
    let later = View { polygon_time: NOW + 60, polygon_block: BLOCK + 40, ..view() };
    let (s2, a) = run(&t, &s, &later);
    assert_eq!(s2, s);
    assert_eq!(a, vec![Action::LockPolygon { deadline_time: dt, deadline_block: db, gas_drop: t.gas_drop }]);
    // Too late to land safely: stop sending.
    let (s2, a) = run(&t, &s, &View { liquid_tip: t.lock_height - 389, ..later.clone() });
    assert_eq!((s2, a), (s.clone(), vec![]));
    // The escrow would refuse it now, on either clock.
    assert_eq!(run(&t, &s, &View { polygon_time: dt, ..later.clone() }), (State::Abandoned, vec![]));
    assert_eq!(run(&t, &s, &View { polygon_block: db, ..later }), (State::Abandoned, vec![]));
}

#[test]
fn l2p_reads_the_secret_from_the_escrow_and_claims_until_confirmed() {
    let t = terms(Direction::L2P);
    let (dt, db) = l2p_deadlines(&cfg(), &view());
    let s = State::LockedPolygon { deadline_time: dt, deadline_block: db };
    let claimed = EscrowView { state: EscrowState::Claimed, revealed: polygon_value(&P).unwrap(), ..maker_escrow() };
    let v = View { escrow: Some(claimed), htlc: coin(5), ..view() };
    let (s2, a) = run(&t, &s, &v);
    assert_eq!(s2, State::Claiming { secret: P });
    assert_eq!(a, vec![Action::ClaimLiquid { secret: P }]);
    let mempool = View { htlc: HtlcView::SpentByClaim { secret: P, confirmed: false }, ..v.clone() };
    assert_eq!(run(&t, &s2, &mempool), (s2.clone(), vec![Action::ClaimLiquid { secret: P }]));
    let done = View { htlc: HtlcView::SpentByClaim { secret: P, confirmed: true }, ..v };
    assert_eq!(run(&t, &s2, &done), (State::Done { outcome: Outcome::Completed }, vec![]));
}

#[test]
fn l2p_ignores_a_stored_secret_that_does_not_open_the_point() {
    let t = terms(Direction::L2P);
    let (dt, db) = l2p_deadlines(&cfg(), &view());
    let s = State::LockedPolygon { deadline_time: dt, deadline_block: db };
    let bad = EscrowView { state: EscrowState::Claimed, revealed: polygon_value(&[9; 32]).unwrap(), ..maker_escrow() };
    let (s2, a) = run(&t, &s, &View { escrow: Some(bad), htlc: coin(5), ..view() });
    assert_eq!(s2, s);
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
}

#[test]
fn l2p_pages_when_the_claim_is_urgent_and_when_it_is_lost() {
    let t = terms(Direction::L2P);
    let s = State::Claiming { secret: P };
    let (_, a) = run(&t, &s, &View { htlc: coin(5), liquid_tip: t.lock_height - 60, ..view() });
    assert!(a.contains(&Action::Page(Page::ClaimUrgent { left: 60 })));
    let (_, a) = run(&t, &s, &View { htlc: coin(5), liquid_tip: t.lock_height - 61, ..view() });
    assert_eq!(a, vec![Action::ClaimLiquid { secret: P }]);
    let (s2, a) = run(&t, &s, &View { htlc: HtlcView::SpentByRefund { confirmed: true }, ..view() });
    assert_eq!((s2, a), (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)]));
}

/// The Polygon-halt case of docs/design.md section 3.4: after a halt the timestamp jumps, and our short lock refunds on the
/// time clock alone. Against a slow clock, the block clock does it.
#[test]
fn l2p_refunds_when_either_clock_closes_the_claim_and_prefers_a_claim_that_landed() {
    let t = terms(Direction::L2P);
    let open = maker_escrow();
    let s = State::LockedPolygon { deadline_time: open.deadline_time, deadline_block: open.deadline_block };
    let at = |time: u64, block: u64| View { escrow: Some(open.clone()), polygon_time: time, polygon_block: block, ..view() };
    let (dt, db) = (open.deadline_time, open.deadline_block);
    assert_eq!(run(&t, &s, &at(dt - 1, db - 1)), (s.clone(), vec![]));
    let refunding = (State::Refunding, vec![Action::RefundPolygon]);
    assert_eq!(run(&t, &s, &at(dt, BLOCK + 10)), refunding, "a halt: time jumped, blocks didn't");
    assert_eq!(run(&t, &s, &at(NOW + 3_600, db)), refunding, "a slow clock: blocks went on, time lagged");

    let done = EscrowView { state: EscrowState::Refunded, ..open.clone() };
    assert_eq!(run(&t, &State::Refunding, &View { escrow: Some(done), ..view() }).0, State::Done { outcome: Outcome::Refunded });
    let claimed = EscrowView { state: EscrowState::Claimed, revealed: polygon_value(&P).unwrap(), ..open };
    let (s3, a) = run(&t, &State::Refunding, &View { escrow: Some(claimed), htlc: coin(5), ..view() });
    assert_eq!((s3, a), (State::Claiming { secret: P }, vec![Action::ClaimLiquid { secret: P }]));
}

// ---- P to L ----

#[test]
fn p2l_locks_liquid_once_the_escrow_matches() {
    let t = terms(Direction::P2L);
    let (s, a) = run(&t, &State::WatchLock, &View { escrow: Some(taker_escrow()), ..view() });
    assert_eq!((s, a), (State::LockedLiquid, vec![Action::LockLiquid]));
}

#[test]
fn p2l_will_not_lock_without_each_condition() {
    let t = terms(Direction::P2L);
    let e = taker_escrow();
    let good = View { escrow: Some(e.clone()), ..view() };
    let gate = cfg().p2l_gate_secs;
    let bad = [
        View { escrow: None, ..good.clone() },
        View { escrow: Some(EscrowView { amount: USDT - 1, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { lock_address_ok: false, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { recipient_ok: false, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { long_leg: false, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { extended: true, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { state: EscrowState::Refunded, ..e.clone() }), ..good.clone() },
        // Each clock must leave the gate's time by itself.
        View { escrow: Some(EscrowView { deadline_time: NOW + gate - 1, ..e.clone() }), ..good.clone() },
        View { escrow: Some(EscrowView { deadline_block: BLOCK + blocks_for(gate, MS) - 1, ..e.clone() }), ..good.clone() },
        View { maker_unblocked: false, ..good.clone() },
        View { htlc: coin(0), ..good.clone() },
        View { liquid_tip_age_secs: 601, ..good.clone() },
        View { polygon_age_secs: 61, ..good.clone() },
        View { polygon_lag_blocks: 101, ..good.clone() },
        View { sources_disagree: true, ..good.clone() },
        View { open_exposure: cfg().max_open_p2l, ..good.clone() },
    ];
    for v in bad {
        let (_, a) = run(&t, &State::WatchLock, &v);
        assert!(!a.contains(&Action::LockLiquid), "{v:?}");
    }
    let edge = EscrowView { deadline_time: NOW + gate, deadline_block: BLOCK + blocks_for(gate, MS), ..e };
    assert!(p2l_may_lock(&cfg(), &t, &View { escrow: Some(edge), ..good }));
}

#[test]
fn p2l_past_the_window_abandons_and_releases_the_taker() {
    let t = terms(Direction::P2L);
    let v = View { liquid_tip: ACCEPT + 31, escrow: Some(taker_escrow()), ..view() };
    assert_eq!(run(&t, &State::WatchLock, &v), (State::Abandoned, vec![Action::Release]));
    let other = View { escrow: Some(EscrowView { recipient_ok: false, ..taker_escrow() }), ..v.clone() };
    assert_eq!(run(&t, &State::WatchLock, &other), (State::Abandoned, vec![]));
    assert_eq!(run(&t, &State::WatchLock, &View { escrow: None, ..v }), (State::Abandoned, vec![]));
}

#[test]
fn p2l_only_ever_resends_the_same_lock() {
    let t = terms(Direction::P2L);
    let (s, a) = run(&t, &State::LockedLiquid, &View { escrow: Some(taker_escrow()), ..view() });
    assert_eq!((s, a), (State::LockedLiquid, vec![Action::RebroadcastLock]));
    let (_, a) = run(&t, &State::LockedLiquid, &View { liquid_tip: t.lock_height, escrow: Some(taker_escrow()), ..view() });
    assert_eq!(a, vec![Action::Page(Page::LockMissing)]);
}

#[test]
fn p2l_reads_the_secret_from_a_mempool_claim_and_collects() {
    let t = terms(Direction::P2L);
    let v = View { htlc: HtlcView::SpentByClaim { secret: P, confirmed: false }, escrow: Some(taker_escrow()), ..view() };
    let (s, a) = run(&t, &State::LockedLiquid, &v);
    assert_eq!((s.clone(), a), (State::Claiming { secret: P }, vec![Action::ClaimPolygon { secret: P }]));
    let claimed = EscrowView { state: EscrowState::Claimed, ..taker_escrow() };
    assert_eq!(run(&t, &s, &View { escrow: Some(claimed), ..v.clone() }).0, State::Done { outcome: Outcome::Completed });
    let refunded = EscrowView { state: EscrowState::Refunded, ..taker_escrow() };
    assert_eq!(
        run(&t, &s, &View { escrow: Some(refunded), ..v.clone() }),
        (State::Done { outcome: Outcome::Lost }, vec![Action::Page(Page::Lost)])
    );
    // An unreadable lock is never taken to mean anything: keep claiming.
    assert_eq!(run(&t, &s, &View { escrow: None, ..v }), (s.clone(), vec![Action::ClaimPolygon { secret: P }]));
}

#[test]
fn p2l_rejects_a_claim_witness_that_does_not_open_the_point() {
    let t = terms(Direction::P2L);
    let v =
        View { htlc: HtlcView::SpentByClaim { secret: [9; 32], confirmed: true }, escrow: Some(taker_escrow()), ..view() };
    let (s, a) = run(&t, &State::LockedLiquid, &v);
    assert_eq!(s, State::LockedLiquid);
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
}

#[test]
fn p2l_pages_when_an_unclaimed_secret_nears_both_deadlines() {
    let t = terms(Direction::P2L);
    let s = State::Claiming { secret: P };
    // Near on time, but the block clock keeps a long leg open for hours: no page yet.
    let time_only = EscrowView { deadline_time: NOW + 3_600, ..taker_escrow() };
    assert_eq!(run(&t, &s, &View { escrow: Some(time_only), ..view() }).1, vec![Action::ClaimPolygon { secret: P }]);
    let both = EscrowView { deadline_time: NOW + 3_600, deadline_block: BLOCK + 2_000, ..taker_escrow() };
    let (_, a) = run(&t, &s, &View { escrow: Some(both), ..view() });
    assert!(a.contains(&Action::Page(Page::ClaimUrgent { left: 3_600 })), "{a:?}");
}

#[test]
fn p2l_refund_timing_and_a_late_claim_racing_it() {
    let t = terms(Direction::P2L);
    let e = Some(taker_escrow());
    let (s, a) =
        run(&t, &State::LockedLiquid, &View { liquid_tip: t.lock_height - 1, htlc: coin(5), escrow: e.clone(), ..view() });
    assert_eq!((s, a), (State::LockedLiquid, vec![]));
    let (s, a) = run(&t, &State::LockedLiquid, &View { liquid_tip: t.lock_height, htlc: coin(5), escrow: e.clone(), ..view() });
    assert_eq!((s.clone(), a), (State::Refunding, vec![Action::RefundLiquid]));
    let late = View { htlc: HtlcView::SpentByClaim { secret: P, confirmed: false }, escrow: e.clone(), ..view() };
    assert_eq!(run(&t, &s, &late), (State::Claiming { secret: P }, vec![Action::ClaimPolygon { secret: P }]));
    let mine = View { htlc: HtlcView::SpentByRefund { confirmed: false }, escrow: e.clone(), ..view() };
    assert_eq!(run(&t, &s, &mine), (State::Refunding, vec![Action::RefundLiquid]));
    let done = View { htlc: HtlcView::SpentByRefund { confirmed: true }, escrow: e, ..view() };
    assert_eq!(run(&t, &s, &done).0, State::Done { outcome: Outcome::Refunded });
}

#[test]
fn p2l_extends_both_clocks_only_for_a_long_stall_near_the_deadline() {
    let t = terms(Direction::P2L);
    let near = EscrowView { deadline_time: NOW + 3_600, deadline_block: BLOCK + 2_400, ..taker_escrow() };
    let v = View { liquid_tip_age_secs: 3_600, htlc: coin(5), escrow: Some(near.clone()), ..view() };
    // Seven days, and 403,200 blocks at 1.5 s: exactly the escrow's limit on mainnet.
    let want = Action::Extend { new_time: NOW + 3_600 + 604_800, new_block: BLOCK + 2_400 + 403_200 };
    assert_eq!(run(&t, &State::LockedLiquid, &v).1, vec![want]);
    // Faster blocks would need more, but the escrow's limit caps it.
    let fast = View { polygon_block_ms: 1_000, ..v.clone() };
    let near_fast = EscrowView { deadline_block: BLOCK + 3_600, ..near.clone() };
    let (_, a) = run(&t, &State::LockedLiquid, &View { escrow: Some(near_fast), ..fast });
    assert_eq!(a, vec![Action::Extend { new_time: NOW + 3_600 + 604_800, new_block: BLOCK + 3_600 + 403_200 }]);
    let no = |v: View| assert_eq!(run(&t, &State::LockedLiquid, &v).1, vec![], "{v:?}");
    no(View { liquid_tip_age_secs: 3_599, ..v.clone() });
    no(View { escrow: Some(EscrowView { deadline_time: NOW + 3_601, ..near.clone() }), ..v.clone() });
    no(View { escrow: Some(EscrowView { deadline_block: BLOCK + 2_401, ..near.clone() }), ..v.clone() });
    no(View { escrow: Some(EscrowView { extended: true, ..near.clone() }), ..v.clone() });
    no(View { escrow: Some(EscrowView { long_leg: false, ..near.clone() }), ..v.clone() });
    no(View { polygon_time: near.deadline_time, polygon_block: near.deadline_block, ..v });
}

// ---- Both ----

#[test]
fn rollback_stops_everything_and_pages() {
    for d in [Direction::L2P, Direction::P2L] {
        let t = terms(d);
        for s in [
            State::WatchLock,
            State::LockedLiquid,
            State::LockedPolygon { deadline_time: 1, deadline_block: 1 },
            State::Claiming { secret: P },
            State::Refunding,
        ] {
            let v = View { rollback_detected: true, htlc: coin(9), escrow: Some(taker_escrow()), liquid_tip: t.lock_height, ..view() };
            assert_eq!(run(&t, &s, &v), (s.clone(), vec![Action::Page(Page::Rollback)]));
        }
    }
}

/// liq2sol's protocol section 7.3: after a rollback, an honest maker that collected on Polygon must leave the coin to the taker.
#[test]
fn a_finished_swap_never_refunds_even_if_the_coin_comes_back() {
    let t = terms(Direction::P2L);
    let done = State::Done { outcome: Outcome::Completed };
    let claimed = EscrowView { state: EscrowState::Claimed, ..taker_escrow() };
    let v = View { liquid_tip: t.lock_height + 100, htlc: coin(9), escrow: Some(claimed), ..view() };
    assert_eq!(run(&t, &done, &v), (done.clone(), vec![]));
    assert_eq!(run(&t, &State::LockedLiquid, &v), (State::LockedLiquid, vec![]));
    assert_eq!(run(&t, &State::Refunding, &v), (State::Refunding, vec![]));
}

#[test]
fn a_state_from_the_other_direction_pages() {
    let (s, a) = run(&terms(Direction::L2P), &State::LockedLiquid, &view());
    assert_eq!(s, State::LockedLiquid);
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
    let (_, a) = run(&terms(Direction::P2L), &State::LockedPolygon { deadline_time: 1, deadline_block: 1 }, &view());
    assert!(matches!(a.as_slice(), [Action::Page(Page::Impossible(_))]));
}

// ---- Invariants under arbitrary inputs ----

fn any_escrow() -> impl Strategy<Value = Option<EscrowView>> {
    proptest::option::of(
        (0u8..3, 0u64..3, any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>(), 0u64..40_000, 0u64..30_000, any::<bool>())
            .prop_map(
                |(state, short, lock_ok, recipient_ok, long_leg, extended, dt, db, good_secret)| EscrowView {
                    state: [EscrowState::Open, EscrowState::Claimed, EscrowState::Refunded][state as usize],
                    amount: USDT - short,
                    lock_address_ok: lock_ok,
                    recipient_ok,
                    deadline_time: NOW + dt,
                    deadline_block: BLOCK + db,
                    long_leg,
                    extended,
                    revealed: polygon_value(&if good_secret { P } else { [3; 32] }).unwrap(),
                },
            ),
    )
}

fn any_view() -> impl Strategy<Value = View> {
    (
        (900u32..1_600, 0u64..5_000, 0u64..40_000, 0u64..30_000, 0u64..200, 0u64..300, 400u64..6_000),
        (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()),
        0u8..5,
        any::<bool>(),
        any_escrow(),
        0u64..6_000_000_000,
    )
        .prop_map(
            |(
                (liquid_tip, liquid_tip_age_secs, dt, db, polygon_age_secs, polygon_lag_blocks, polygon_block_ms),
                (rollback_detected, sources_disagree, maker_unblocked, taker_unblocked),
                htlc,
                flag,
                escrow,
                open_exposure,
            )| View {
                liquid_tip,
                liquid_tip_age_secs,
                polygon_time: NOW + dt,
                polygon_block: BLOCK + db,
                polygon_age_secs,
                polygon_lag_blocks,
                polygon_block_ms,
                rollback_detected,
                sources_disagree,
                maker_unblocked,
                taker_unblocked,
                adaptor_held: flag,
                htlc: match htlc {
                    0 => HtlcView::Absent,
                    1 => HtlcView::Unspent { confirmations: u32::from(flag) * 5 },
                    2 => HtlcView::SpentByClaim { secret: P, confirmed: flag },
                    3 => HtlcView::SpentByClaim { secret: [3; 32], confirmed: flag },
                    _ => HtlcView::SpentByRefund { confirmed: flag },
                },
                escrow,
                open_exposure,
            },
        )
}

fn is_safe_secret(a: &Action) -> bool {
    match a {
        Action::ClaimLiquid { secret } | Action::ClaimPolygon { secret } => *secret == P,
        _ => true,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    /// Run whole random histories, and check the rules that must never break.
    #[test]
    fn invariants_hold_over_random_histories(p2l in any::<bool>(), views in proptest::collection::vec(any_view(), 1..25)) {
        let t = terms(if p2l { Direction::P2L } else { Direction::L2P });
        let c = cfg();
        let mut state = State::WatchLock;
        let mut lock_liquid_count = 0;
        let mut collected = false;
        let mut lock_deadlines = None;
        for v in &views {
            let before = state.clone();
            let (next, actions) = step(&c, &t, &state, v);

            for a in &actions {
                prop_assert!(is_safe_secret(a), "only a correct secret is ever used: {a:?}");
                if v.rollback_detected {
                    prop_assert!(matches!(a, Action::Page(_)), "a rollback stops everything but pages: {a:?}");
                }
                match a {
                    Action::LockLiquid => {
                        lock_liquid_count += 1;
                        prop_assert!(p2l && p2l_may_lock(&c, &t, v) && before == State::WatchLock);
                    }
                    Action::LockPolygon { deadline_time, deadline_block, gas_drop } => {
                        prop_assert!(!p2l);
                        prop_assert!(*gas_drop == t.gas_drop && *gas_drop <= c.max_gas_drop);
                        if before == State::WatchLock {
                            prop_assert!(l2p_may_lock(&c, &t, v));
                        }
                        // Every retry keeps the first deadlines.
                        let first = *lock_deadlines.get_or_insert((*deadline_time, *deadline_block));
                        prop_assert_eq!(first, (*deadline_time, *deadline_block));
                        prop_assert!(t.lock_height.saturating_sub(v.liquid_tip) >= c.l2p_gate_blocks, "never lock inside the margin");
                    }
                    Action::RefundLiquid => {
                        prop_assert!(p2l && v.liquid_tip >= t.lock_height || before == State::Refunding);
                        prop_assert!(!collected, "never refund a coin after collecting its lock");
                        prop_assert!(!v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed));
                    }
                    Action::RefundPolygon => {
                        prop_assert!(!p2l);
                        if before != State::Refunding {
                            let e = v.escrow.as_ref().expect("a refund needs our lock");
                            prop_assert!(!e.can_claim(v.polygon_time, v.polygon_block), "never refund while the taker may claim");
                        }
                    }
                    Action::Release => {
                        prop_assert!(before == State::WatchLock && lock_liquid_count == 0, "never release after locking");
                    }
                    Action::Extend { new_time, new_block } => {
                        let e = v.escrow.as_ref().expect("extend needs an escrow");
                        prop_assert!(p2l && e.long_leg && !e.extended && e.state == EscrowState::Open);
                        prop_assert!(*new_time > e.deadline_time && *new_time - e.deadline_time <= c.extend_by_secs);
                        prop_assert!(*new_block > e.deadline_block && *new_block - e.deadline_block <= c.max_extension_blocks);
                        prop_assert!(v.liquid_tip_age_secs >= c.extend_after_stall_secs);
                    }
                    _ => {}
                }
            }
            prop_assert!(lock_liquid_count <= 1, "the Liquid lock can't be paid twice");

            if matches!(before, State::Done { .. } | State::Abandoned) {
                prop_assert_eq!(&next, &before, "finished is final");
                prop_assert!(actions.is_empty(), "a finished swap does nothing");
            }
            if v.rollback_detected {
                prop_assert_eq!(&next, &before, "a rollback changes no state");
            }
            if v.escrow.as_ref().is_some_and(|e| e.state == EscrowState::Claimed) && p2l {
                collected = true;
            }
            state = next;
        }
    }
}
