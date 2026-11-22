//! The signer must refuse out-of-policy requests by itself, whatever a compromised engine asks for. From liq2sol's, with
//! Polygon's rules added (docs/design.md section 4.2).

use std::cell::RefCell;
use std::collections::HashMap;

use liq2any_maker_core::engine::{blocks_for, Config, Direction};
use liq2any_maker_core::policy::*;
use liq2any_maker_core::store::{SignedCall, SignerLedger};
use liq2any_maker_core::LockPoint;
use liq2any_swapkey::point_of;

const P: [u8; 32] = [7; 32];
const NOW: u64 = 1_000_000;
const TIME: u64 = 1_800_000_000;
const BLOCK: u64 = 50_000_000;
const MS: u64 = 1_500;
const DROP: u128 = 100_000_000_000_000_000;

#[derive(Default)]
struct Mem {
    records: RefCell<HashMap<[u8; 32], SwapRecord>>,
    used: u64,
}

impl Ledger for Mem {
    fn record(&self, hash: &[u8; 32]) -> SwapRecord {
        self.records.borrow().get(hash).cloned().unwrap_or_default()
    }
    fn locked_in_window(&self, _: u64, _: u64) -> u64 {
        self.used
    }
}

fn limits() -> Limits {
    Limits::from(&Config::default())
}

fn point() -> LockPoint {
    LockPoint(point_of(&P).unwrap())
}

fn hash() -> [u8; 32] {
    point().id()
}

/// A swap with its adapter signature already stored, the only state a Polygon lock can start from.
fn held() -> Mem {
    let m = Mem::default();
    m.records.borrow_mut().insert(hash(), SwapRecord { adaptor_held: true, ..SwapRecord::default() });
    m
}

const AMOUNT: u64 = 100;

fn coin(amount: u64, confirmations: u32) -> Option<CoinEvidence> {
    Some(CoinEvidence { amount, confirmations })
}

fn clock() -> PolygonClock {
    PolygonClock { time: TIME, block: BLOCK, block_ms: MS }
}

/// Three hours on the time clock, and 7,200 blocks plus 10% on the block clock.
const DEADLINE_SECS: u64 = 10_800;
const DEADLINE_BLOCKS: u64 = 7_920;

#[allow(clippy::too_many_arguments)]
fn lock_polygon_full(
    amount: u64,
    secs: u64,
    blocks: u64,
    gas_drop: u128,
    clock: PolygonClock,
    liquid_lock_height: u32,
    taker_coin: Option<CoinEvidence>,
    recipient_blocked: bool,
) -> Request {
    Request::LockPolygon {
        swap_id: hash(),
        amount,
        deadline_time: clock.time + secs,
        deadline_block: clock.block + blocks,
        gas_drop,
        clock,
        liquid_lock_height,
        liquid_tip: 1_000,
        taker_coin,
        recipient_blocked,
    }
}

fn lock_polygon_with(amount: u64, secs: u64, liquid_lock_height: u32, taker_coin: Option<CoinEvidence>) -> Request {
    lock_polygon_full(amount, secs, DEADLINE_BLOCKS, DROP, clock(), liquid_lock_height, taker_coin, false)
}

fn lock_polygon() -> Request {
    lock_polygon_with(AMOUNT, DEADLINE_SECS, 1_480, coin(AMOUNT * 100, 2))
}

fn escrow(amount: u64, matches_terms: bool, secs_left: u64) -> Option<EscrowEvidence> {
    Some(EscrowEvidence { amount, matches_terms, secs_left_time: secs_left, secs_left_blocks: secs_left })
}

fn lock_liquid_full(liquid_amount: u64, lock_height: u32, taker_escrow: Option<EscrowEvidence>, maker_blocked: bool) -> Request {
    Request::LockLiquid { swap_id: hash(), liquid_amount, lock_height, liquid_tip: 1_000, taker_escrow, maker_blocked }
}

fn lock_liquid_with(liquid_amount: u64, lock_height: u32, taker_escrow: Option<EscrowEvidence>) -> Request {
    lock_liquid_full(liquid_amount, lock_height, taker_escrow, false)
}

fn lock_liquid() -> Request {
    lock_liquid_with(AMOUNT * 100, 1_080, escrow(AMOUNT, true, 28_800))
}

/// Apply a request as the signer would: check, then remember.
fn apply(m: &Mem, r: &Request) -> Result<(), Refusal> {
    let rec = check(&limits(), m, NOW, r)?;
    m.records.borrow_mut().insert(hash(), rec);
    Ok(())
}

#[test]
fn a_lock_is_refused_outside_its_limits() {
    let m = held();
    let big = limits().max_per_swap + 1;
    assert_eq!(
        check(&limits(), &m, NOW, &lock_polygon_with(big, DEADLINE_SECS, 1_480, coin(big * 100, 9))),
        Err(Refusal::OverSwapLimit)
    );
    let full = Mem { used: limits().max_per_window, ..held() };
    assert_eq!(check(&limits(), &full, NOW, &lock_polygon()), Err(Refusal::OverWindowLimit));
    let almost = Mem { used: u64::MAX, ..held() };
    assert_eq!(check(&limits(), &almost, NOW, &lock_polygon()), Err(Refusal::OverWindowLimit), "no overflow");
}

/// A compromised engine locking our funds to a colluding taker who never paid.
#[test]
fn a_polygon_lock_needs_the_takers_coin_confirmed() {
    let m = held();
    let got = |c| check(&limits(), &m, NOW, &lock_polygon_with(AMOUNT, DEADLINE_SECS, 1_480, c));
    assert_eq!(got(None), Err(Refusal::NoTakerCoin));
    assert_eq!(got(coin(AMOUNT * 100, 1)), Err(Refusal::NoTakerCoin));
    assert!(got(coin(AMOUNT * 100, 2)).is_ok());

    let large = limits().large_above + 1;
    let wide = Limits { max_per_swap: u64::MAX, max_per_window: u64::MAX, ..limits() };
    let at = |confs| check(&wide, &m, NOW, &lock_polygon_with(large, DEADLINE_SECS, 1_480, coin(large * 100, confs)));
    assert_eq!(at(9), Err(Refusal::NoTakerCoin));
    assert!(at(10).is_ok());
}

/// A compromised engine paying out more than the coin it receives.
#[test]
fn a_polygon_lock_is_refused_at_a_loss() {
    let m = held();
    let got = |c| check(&limits(), &m, NOW, &lock_polygon_with(AMOUNT, DEADLINE_SECS, 1_480, c));
    assert_eq!(got(coin(AMOUNT * 100 - 1, 2)), Err(Refusal::BadPrice));
    assert!(got(coin(AMOUNT * 100, 2)).is_ok());
    let wide = Limits { max_per_swap: u64::MAX, max_per_window: u64::MAX, ..limits() };
    let overflow = lock_polygon_with(u64::MAX, DEADLINE_SECS, 1_480, coin(u64::MAX, 99));
    assert_eq!(check(&wide, &m, NOW, &overflow), Err(Refusal::BadPrice), "no overflow");
}

#[test]
fn polygon_lock_deadlines_are_checked_on_both_clocks() {
    let m = held();
    let c = coin(AMOUNT * 100, 2);
    let got = |secs, blocks, height| {
        check(&limits(), &m, NOW, &lock_polygon_full(AMOUNT, secs, blocks, DROP, clock(), height, c, false))
    };
    assert!(got(DEADLINE_SECS, DEADLINE_BLOCKS, 1_480).is_ok());
    assert!(matches!(got(DEADLINE_SECS + 1, DEADLINE_BLOCKS, 1_480), Err(Refusal::BadTimeout(_))));
    assert!(matches!(got(0, DEADLINE_BLOCKS, 1_480), Err(Refusal::BadTimeout(_))));
    // The block clock may run past the time clock by the slack, and no further.
    assert!(matches!(got(DEADLINE_SECS, DEADLINE_BLOCKS + 1, 1_480), Err(Refusal::BadTimeout(_))));
    assert!(matches!(got(DEADLINE_SECS, 0, 1_480), Err(Refusal::BadTimeout(_))));
    assert!(got(DEADLINE_SECS, 1, 1_480).is_ok(), "a shorter block clock only shortens our own exposure");
    assert!(matches!(got(DEADLINE_SECS, DEADLINE_BLOCKS, 1_389), Err(Refusal::BadTimeout(_))));
    assert!(got(DEADLINE_SECS, DEADLINE_BLOCKS, 1_390).is_ok());
}

#[test]
fn a_polygon_lock_needs_a_plausible_block_time() {
    let m = held();
    let c = coin(AMOUNT * 100, 2);
    for ms in [0, 499, 5_001, u64::MAX] {
        let odd = PolygonClock { block_ms: ms, ..clock() };
        let blocks = blocks_for(DEADLINE_SECS, ms.max(1));
        let r = check(&limits(), &m, NOW, &lock_polygon_full(AMOUNT, DEADLINE_SECS, blocks.max(1), DROP, odd, 1_480, c, false));
        assert!(matches!(r, Err(Refusal::BadTimeout(_))), "{ms} ms gave {r:?}");
    }
}

/// A claim would still pay a blocked recipient, whose coins would then be stuck (docs/assumptions.md section 1.2).
#[test]
fn a_polygon_lock_to_a_blocked_taker_is_refused() {
    let m = held();
    let blocked = lock_polygon_full(AMOUNT, DEADLINE_SECS, DEADLINE_BLOCKS, DROP, clock(), 1_480, coin(AMOUNT * 100, 2), true);
    assert_eq!(check(&limits(), &m, NOW, &blocked), Err(Refusal::Blocked));
}

#[test]
fn the_gas_drop_has_a_cap() {
    let m = held();
    let c = coin(AMOUNT * 100, 2);
    let at = |drop| check(&limits(), &m, NOW, &lock_polygon_full(AMOUNT, DEADLINE_SECS, DEADLINE_BLOCKS, drop, clock(), 1_480, c, false));
    assert!(at(limits().max_gas_drop).is_ok());
    assert_eq!(at(limits().max_gas_drop + 1), Err(Refusal::GasDropTooLarge));
}

/// A compromised engine locking on Liquid against a Polygon lock that isn't there or doesn't pay us.
#[test]
fn a_liquid_lock_needs_a_matching_escrow_on_both_clocks() {
    let m = held();
    let liq = AMOUNT * 100;
    let gate = limits().p2l_gate_secs;
    let got = |e| check(&limits(), &m, NOW, &lock_liquid_with(liq, 1_080, e));
    assert_eq!(got(None), Err(Refusal::NoTakerEscrow));
    assert_eq!(got(escrow(AMOUNT, false, 28_800)), Err(Refusal::NoTakerEscrow));
    assert_eq!(got(escrow(AMOUNT, true, gate - 1)), Err(Refusal::NoTakerEscrow));
    let time_short = Some(EscrowEvidence { amount: AMOUNT, matches_terms: true, secs_left_time: gate - 1, secs_left_blocks: 28_800 });
    assert_eq!(got(time_short), Err(Refusal::NoTakerEscrow), "a halt would eat the time clock");
    let blocks_short = Some(EscrowEvidence { amount: AMOUNT, matches_terms: true, secs_left_time: 28_800, secs_left_blocks: gate - 1 });
    assert_eq!(got(blocks_short), Err(Refusal::NoTakerEscrow), "faster blocks would eat the block clock");
    assert_eq!(got(escrow(AMOUNT - 1, true, 28_800)), Err(Refusal::BadPrice));
    assert!(got(escrow(AMOUNT, true, gate)).is_ok());
    let odd = lock_liquid_with(liq - 1, 1_080, escrow(AMOUNT, true, 28_800));
    assert_eq!(check(&limits(), &m, NOW, &odd), Err(Refusal::BadPrice), "must be an exact multiple of 100");
}

#[test]
fn a_liquid_lock_is_refused_if_we_are_blocked() {
    let m = held();
    let r = lock_liquid_full(AMOUNT * 100, 1_080, escrow(AMOUNT, true, 28_800), true);
    assert_eq!(check(&limits(), &m, NOW, &r), Err(Refusal::Blocked));
}

#[test]
fn a_second_different_lock_is_refused_but_an_identical_retry_is_signed() {
    let m = held();
    apply(&m, &lock_polygon()).unwrap();
    apply(&m, &lock_polygon()).expect("identical retry");
    let c = coin(AMOUNT * 100, 2);
    for different in [
        lock_polygon_full(AMOUNT, DEADLINE_SECS - 1, DEADLINE_BLOCKS, DROP, clock(), 1_480, c, false),
        lock_polygon_full(AMOUNT, DEADLINE_SECS, DEADLINE_BLOCKS - 1, DROP, clock(), 1_480, c, false),
        lock_polygon_full(AMOUNT, DEADLINE_SECS, DEADLINE_BLOCKS, DROP + 1, clock(), 1_480, c, false),
        // A "retry" for a bigger amount is a second lock, and would slip past every lock limit.
        lock_polygon_full(AMOUNT + 1, DEADLINE_SECS, DEADLINE_BLOCKS, DROP, clock(), 1_480, coin((AMOUNT + 1) * 100, 2), false),
    ] {
        assert_eq!(apply(&m, &different), Err(Refusal::AlreadyLocked), "{different:?}");
    }
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked));

    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::AlreadyLocked), "a Liquid lock is never signed twice");
}

#[test]
fn liquid_lock_height_must_be_in_range() {
    let m = held();
    let at = |h| check(&limits(), &m, NOW, &lock_liquid_with(AMOUNT * 100, h, escrow(AMOUNT, true, 28_800)));
    assert!(matches!(at(1_029), Err(Refusal::BadTimeout(_))));
    assert!(matches!(at(1_121), Err(Refusal::BadTimeout(_))));
    assert!(at(1_030).is_ok());
    assert!(at(1_120).is_ok());
}

#[test]
fn claims_need_a_known_swap_and_the_right_secret() {
    let m = held();
    let claim = |secret| Request::ClaimLiquid { lock_point: point(), secret };
    assert_eq!(apply(&m, &claim(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_polygon()).unwrap();
    assert_eq!(apply(&m, &claim([1; 32])), Err(Refusal::WrongSecret));
    apply(&m, &claim(P)).unwrap();

    let m = held();
    let poly = |secret| Request::ClaimPolygon { lock_point: point(), secret };
    assert_eq!(apply(&m, &poly(P)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &poly([1; 32])), Err(Refusal::WrongSecret));
    apply(&m, &poly(P)).unwrap();
}

fn refund(liquid_tip: u32) -> Request {
    Request::RefundLiquid { swap_id: hash(), lock_height: 1_090, liquid_tip }
}

/// A compromised engine trying to take both sides after a rollback.
#[test]
fn liquid_refund_is_refused_after_collecting_on_polygon() {
    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    apply(&m, &Request::ClaimPolygon { lock_point: point(), secret: P }).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::RefundAfterClaim));
}

#[test]
fn liquid_refund_rules() {
    let m = held();
    assert_eq!(apply(&m, &refund(1_090)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert!(matches!(apply(&m, &refund(1_089)), Err(Refusal::BadTimeout(_))));
    apply(&m, &refund(1_090)).unwrap();
}

/// A compromised engine handing a colluding taker both sides.
#[test]
fn release_is_refused_once_we_have_locked() {
    let m = held();
    let release = Request::Release { swap_id: hash() };
    assert!(check(&limits(), &m, NOW, &release).is_ok());
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &release), Err(Refusal::ReleaseAfterLock));
}

#[test]
fn extend_needs_a_real_stall_and_a_bounded_move_on_both_clocks() {
    let m = held();
    let ext = |new_time, new_block, liquid_tip_age_secs| Request::Extend {
        swap_id: hash(),
        current_time: 100,
        current_block: 1_000,
        new_time,
        new_block,
        liquid_tip_age_secs,
    };
    let (week, most) = (limits().extend_by_secs, limits().max_extension_blocks);
    assert_eq!(apply(&m, &ext(200, 2_000, 3_600)), Err(Refusal::UnknownSwap));
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &ext(200, 2_000, 3_599)), Err(Refusal::ExtendNotJustified));
    assert!(matches!(apply(&m, &ext(100, 2_000, 3_600)), Err(Refusal::BadTimeout(_))));
    assert!(matches!(apply(&m, &ext(200, 1_000, 3_600)), Err(Refusal::BadTimeout(_))));
    assert!(matches!(apply(&m, &ext(100 + week + 1, 2_000, 3_600)), Err(Refusal::BadTimeout(_))));
    assert!(matches!(apply(&m, &ext(200, 1_000 + most + 1, 3_600)), Err(Refusal::BadTimeout(_))));
    apply(&m, &ext(100 + week, 1_000 + most, 3_600)).unwrap();
}

#[test]
fn wrong_direction_requests_are_refused() {
    let m = held();
    apply(&m, &lock_polygon()).unwrap();
    assert_eq!(apply(&m, &refund(2_000)), Err(Refusal::UnknownSwap));
    assert_eq!(apply(&m, &Request::ClaimPolygon { lock_point: point(), secret: P }), Err(Refusal::UnknownSwap));

    let m = held();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &Request::RefundPolygon { swap_id: hash() }), Err(Refusal::UnknownSwap));
}

#[test]
fn only_a_first_lock_counts_against_the_window() {
    let before = SwapRecord::default();
    assert_eq!(newly_locked(&before, &lock_polygon()), Some(AMOUNT));
    assert_eq!(newly_locked(&before, &lock_liquid()), Some(AMOUNT));
    let locked = SwapRecord { direction: Some(Direction::L2P), locked_amount: AMOUNT, ..SwapRecord::default() };
    assert_eq!(newly_locked(&locked, &lock_polygon()), None);
    assert_eq!(newly_locked(&before, &Request::RefundPolygon { swap_id: hash() }), None);
}

/// A compromised engine locking our funds with no way to collect the taker's coin afterwards.
#[test]
fn a_polygon_lock_needs_a_stored_adapter_signature() {
    let m = Mem::default();
    assert_eq!(apply(&m, &lock_polygon()), Err(Refusal::NoAdaptor));
    apply(&m, &Request::HoldAdaptor { swap_id: hash() }).unwrap();
    apply(&m, &lock_polygon()).unwrap();
}

#[test]
fn an_adapter_signature_is_stored_once_and_only_before_locking() {
    let hold = Request::HoldAdaptor { swap_id: hash() };
    let m = Mem::default();
    apply(&m, &hold).unwrap();
    assert_eq!(apply(&m, &hold), Err(Refusal::AdaptorAlreadyHeld), "a stored signature is never replaced");
    apply(&m, &lock_polygon()).unwrap();
    assert_eq!(apply(&m, &hold), Err(Refusal::AlreadyLocked));
}

fn share(liquid_tip: u32) -> Request {
    Request::GiveShare { swap_id: hash(), lock_height: 1_090, liquid_tip, taker_escrow_open: true }
}

/// A second share would let a taker publish a signature we can't read the secret from, and keep the Polygon side too.
#[test]
fn our_share_is_given_once_and_only_while_the_swap_is_live() {
    let m = Mem::default();
    assert_eq!(apply(&m, &share(1_000)), Err(Refusal::UnknownSwap), "nothing is locked");
    apply(&m, &lock_liquid()).unwrap();
    let refunded = Request::GiveShare { swap_id: hash(), lock_height: 1_090, liquid_tip: 1_000, taker_escrow_open: false };
    assert_eq!(apply(&m, &refunded), Err(Refusal::NoTakerEscrow), "the taker took its refund, so a claim would steal");
    assert!(matches!(apply(&m, &share(1_090)), Err(Refusal::BadTimeout(_))), "the refund path is open");
    apply(&m, &share(1_089)).unwrap();
    assert_eq!(apply(&m, &share(1_089)), Err(Refusal::AdaptorAlreadyHeld));

    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    apply(&m, &refund(1_090)).unwrap();
    assert!(matches!(apply(&m, &share(1_000)), Err(Refusal::BadTimeout(_))), "refunded: the coin is gone");

    let m = held();
    apply(&m, &lock_polygon()).unwrap();
    assert_eq!(apply(&m, &share(1_000)), Err(Refusal::UnknownSwap), "wrong direction");
}

// ---- EVM nonces ----

#[test]
fn a_nonce_carries_one_call_forever() {
    let (a, b) = ([1u8; 32], [2u8; 32]);
    assert!(nonce_rule(None, &a).is_ok(), "a fresh nonce");
    assert!(nonce_rule(Some(&a), &a).is_ok(), "the same call again, to bump its fee");
    assert_eq!(nonce_rule(Some(&a), &b), Err(Refusal::NonceTaken), "never a different call");
}

#[test]
fn secs_left_counts_both_clocks() {
    let c = clock();
    assert_eq!(secs_left(TIME + 100, BLOCK + 100, &c), (100, 150));
    assert_eq!(secs_left(TIME - 1, BLOCK - 1, &c), (0, 0), "past is zero, not wrapped");
}

#[test]
fn the_real_ledger_survives_a_restart_and_keeps_transactions_and_nonces() {
    let dir = std::env::temp_dir().join(format!("liq2any-ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("signer.db");
    let _ = std::fs::remove_file(&path);
    let (call, other) = ([5u8; 32], [6u8; 32]);
    {
        let ledger = SignerLedger::open(&path).unwrap();
        let rec = check(&limits(), &ledger, NOW, &lock_liquid()).unwrap();
        ledger.commit(&hash(), &rec, Some((NOW, AMOUNT)), Some(b"tx"), &["cc:1".to_string()], None).unwrap();
        let rec = check(&limits(), &ledger, NOW, &Request::ClaimPolygon { lock_point: point(), secret: P }).unwrap();
        let signed = |call: [u8; 32], raw: &[u8], fee| SignedCall { nonce: 0, call, raw: raw.to_vec(), max_fee: fee, priority_fee: 1 };
        ledger.commit(&hash(), &rec, None, None, &[], Some(&signed(call, b"raw0", 100))).unwrap();
        // A fee bump: the same call at the same nonce, with new bytes and fees.
        ledger.commit(&hash(), &rec, None, None, &[], Some(&signed(call, b"raw0-bumped", 113))).unwrap();
        // Never another call at that nonce, even past the policy.
        assert!(ledger.commit(&hash(), &rec, None, None, &[], Some(&signed(other, b"evil", 999))).is_err());
    }
    let ledger = SignerLedger::open(&path).unwrap();
    assert_eq!(check(&limits(), &ledger, NOW, &refund(2_000)), Err(Refusal::RefundAfterClaim), "remembered across a restart");
    assert_eq!(check(&limits(), &ledger, NOW, &lock_liquid()), Err(Refusal::AlreadyLocked));
    assert_eq!(ledger.lock_tx(&hash()).unwrap().unwrap(), b"tx");
    let kept = ledger.nonce_call(0).unwrap().unwrap();
    assert_eq!((kept.call, kept.raw.as_slice(), kept.max_fee), (call, &b"raw0-bumped"[..], 113));
    assert_eq!(ledger.nonce_of_call(&call).unwrap(), Some(0));
    assert_eq!(ledger.nonce_of_call(&other).unwrap(), None);
    assert_eq!(ledger.next_nonce().unwrap(), 1);
    assert_eq!(ledger.signed_from(0).unwrap(), vec![kept]);
    assert!(ledger.signed_from(1).unwrap().is_empty());
    assert!(ledger.is_used("cc:1").unwrap() && !ledger.is_used("cc:2").unwrap(), "a signed payment's coins stay marked");
    assert!(ledger.reserve_fee_coin(&hash(), "aa:0").unwrap());
    assert!(!ledger.reserve_fee_coin(&[9; 32], "aa:0").unwrap(), "a coin belongs to one swap");
    let rec = SwapRecord { adaptor_held: true, ..SwapRecord::default() };
    ledger.commit_adaptor(&[8; 32], &rec, "body").unwrap();
    assert!(ledger.commit_adaptor(&[8; 32], &rec, "other").is_err(), "a stored signature is never replaced");
    assert_eq!(ledger.locked_in_window(NOW + 1, 3_600), AMOUNT);
    assert_eq!(ledger.locked_in_window(NOW + 3_601, 3_600), 0, "an old lock leaves the window");
    assert_eq!(ledger.unsettled_p2l_locks().unwrap(), 0, "this one was claimed");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- An early refund and a Polygon lock can never both happen ----

#[test]
fn after_an_early_refund_the_swap_can_never_lock() {
    let m = held();
    apply(&m, &Request::EarlyRefund { swap_id: hash() }).unwrap();
    assert_eq!(apply(&m, &lock_polygon()), Err(Refusal::EarlyRefunded));
    let fresh = Mem::default();
    apply(&fresh, &Request::EarlyRefund { swap_id: hash() }).unwrap();
    assert_eq!(apply(&fresh, &Request::HoldAdaptor { swap_id: hash() }), Err(Refusal::EarlyRefunded));
}

#[test]
fn after_signing_a_lock_there_is_no_early_refund() {
    let m = held();
    apply(&m, &lock_polygon()).unwrap();
    assert_eq!(apply(&m, &Request::EarlyRefund { swap_id: hash() }), Err(Refusal::AlreadyLocked));
}

#[test]
fn an_unreadable_record_refuses_an_early_refund() {
    let m = Mem::default();
    m.records.borrow_mut().insert(hash(), SwapRecord { direction: Some(Direction::P2L), ..SwapRecord::default() });
    assert_eq!(apply(&m, &Request::EarlyRefund { swap_id: hash() }), Err(Refusal::AlreadyLocked));
}

#[test]
fn after_releasing_the_takers_lock_we_never_lock_on_liquid() {
    let m = Mem::default();
    apply(&m, &Request::Release { swap_id: hash() }).unwrap();
    assert_eq!(apply(&m, &lock_liquid()), Err(Refusal::EarlyRefunded));
    apply(&m, &Request::Release { swap_id: hash() }).unwrap();
}

#[test]
fn after_locking_on_liquid_we_never_release() {
    let m = Mem::default();
    apply(&m, &lock_liquid()).unwrap();
    assert_eq!(apply(&m, &Request::Release { swap_id: hash() }), Err(Refusal::ReleaseAfterLock));
}
