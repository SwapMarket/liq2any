//! Restart behaviour: nothing committed is forgotten, and a swap id is never accepted twice.

use liq2any_maker_core::engine::*;
use liq2any_maker_core::policy::SwapRecord;
use liq2any_maker_core::store::{SignerLedger, Store};
use liq2any_maker_core::LockPoint;

fn point(n: u8) -> [u8; 33] {
    let mut p = [n; 33];
    p[0] = 2;
    p
}

fn terms(n: u8) -> Terms {
    Terms {
        direction: Direction::P2L,
        lock_point: LockPoint(point(n)),
        liquid_amount: 100,
        polygon_amount: 1,
        h_accept: 10,
        lock_height: 100,
        gas_drop: 0,
    }
}

fn path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("liq2any-store-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("maker.db")
}

#[test]
fn a_swap_id_is_never_accepted_twice_even_after_finishing() {
    let p = path("once");
    let s = Store::open(&p).unwrap();
    assert!(s.accept(&terms(1), &State::WatchLock).unwrap());
    assert!(!s.accept(&terms(1), &State::WatchLock).unwrap());
    s.commit_step(&[1; 32], &State::Done { outcome: Outcome::Completed }, &[]).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert!(!s.accept(&terms(1), &State::WatchLock).unwrap(), "remembered forever");
}

#[test]
fn state_and_intents_survive_a_restart_together() {
    let p = path("restart");
    let s = Store::open(&p).unwrap();
    s.accept(&terms(1), &State::WatchLock).unwrap();
    s.accept(&terms(2), &State::WatchLock).unwrap();
    let ids = s.commit_step(&[1; 32], &State::LockedLiquid, &[Action::LockLiquid]).unwrap();
    s.commit_step(&[2; 32], &State::Abandoned, &[]).unwrap();
    drop(s);

    let s = Store::open(&p).unwrap();
    assert_eq!(s.state(&[1; 32]).unwrap().unwrap().1, State::LockedLiquid);
    let open = s.open_swaps().unwrap();
    assert_eq!(open.len(), 1, "finished swaps are not reloaded");
    assert_eq!(open[0].0.swap_id(), [1; 32]);
    let pending = s.unfinished_intents().unwrap();
    assert_eq!(pending, vec![(ids[0], [1; 32], Action::LockLiquid)]);
    s.finish_intent(ids[0]).unwrap();
    assert!(s.unfinished_intents().unwrap().is_empty());
}

/// The double-payment guard: a crash between storing and broadcasting can only resend the same bytes.
#[test]
fn only_one_lock_transaction_can_ever_be_stored() {
    let p = path("locktx");
    let s = Store::open(&p).unwrap();
    s.accept(&terms(1), &State::WatchLock).unwrap();
    assert!(s.store_lock_tx(&[1; 32], b"tx").is_err(), "no intent yet");
    s.commit_step(&[1; 32], &State::LockedLiquid, &[Action::LockLiquid]).unwrap();
    s.store_lock_tx(&[1; 32], b"tx").unwrap();
    s.store_lock_tx(&[1; 32], b"tx").unwrap();
    assert!(s.store_lock_tx(&[1; 32], b"different").is_err());
    drop(s);
    let s = Store::open(&p).unwrap();
    assert_eq!(s.stored_lock_tx(&[1; 32]).unwrap().unwrap(), b"tx");
    assert!(s.store_lock_tx(&[1; 32], b"different").is_err(), "still refused after a restart");
}

#[test]
fn the_rollback_alarm_stays_until_cleared() {
    let p = path("alarm");
    let s = Store::open(&p).unwrap();
    assert!(!s.rollback().unwrap());
    s.set_rollback(true).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert!(s.rollback().unwrap());
    s.set_last_tip(50, "gone").unwrap();
    s.set_rollback(false).unwrap();
    assert!(!s.rollback().unwrap());
    assert_eq!(s.last_tip().unwrap(), None, "the old tip is forgotten, or the alarm would return at once");
}

#[test]
fn a_pause_lasts_until_resumed_and_apart_from_the_rollback_alarm() {
    let p = path("pause");
    let s = Store::open(&p).unwrap();
    assert!(!s.paused().unwrap());
    s.set_paused(true).unwrap();
    drop(s);
    let s = Store::open(&p).unwrap();
    assert!(s.paused().unwrap(), "a pause survives a restart");
    assert!(!s.rollback().unwrap(), "and is its own flag");
    s.set_paused(false).unwrap();
    assert!(!s.paused().unwrap());
}

#[test]
fn only_p2l_swaps_locked_on_liquid_and_unsettled_need_pol_kept() {
    let p = path("unsettled").with_file_name("signer.db");
    let ledger = SignerLedger::open(&p).unwrap();
    let record = |direction, locked_amount, polygon_claimed, liquid_refunded| SwapRecord {
        direction: Some(direction),
        locked_amount,
        polygon_claimed,
        liquid_refunded,
        ..SwapRecord::default()
    };
    let swaps = [
        record(Direction::P2L, 10, false, false),
        record(Direction::P2L, 10, true, false),
        record(Direction::P2L, 10, false, true),
        record(Direction::P2L, 0, false, false),
        record(Direction::L2P, 10, false, false),
    ];
    for (n, r) in swaps.iter().enumerate() {
        ledger.commit(&[n as u8; 32], r, None, None, &[], None).unwrap();
    }
    assert_eq!(ledger.unsettled_p2l_locks().unwrap(), 1, "only the locked P→L swap not yet claimed nor refunded");
}

/// The engine works from what it read a moment ago, so it mustn't be able to undo a swap being dropped meanwhile.
#[test]
fn an_abandoned_swap_stays_abandoned_and_starts_nothing() {
    let s = Store::open(&path("abandoned")).unwrap();
    assert!(s.accept(&terms(1), &State::WatchLock).unwrap());
    s.commit_step(&[1; 32], &State::Abandoned, &[]).unwrap();
    let ids = s.commit_step(&[1; 32], &State::WatchLock, &[Action::LockLiquid]).unwrap();
    assert!(ids.is_empty(), "no action starts");
    assert_eq!(s.state(&[1; 32]).unwrap().map(|(_, state)| state), Some(State::Abandoned));
    assert!(s.unfinished_intents().unwrap().is_empty());
}

/// What a report is made of: spread by direction, each cost once, and no day for what finished before days were kept.
#[test]
fn the_report_counts_spread_and_each_cost_once() {
    let s = Store::open(&path("report")).unwrap();
    let (mut l2p, mut p2l, mut refunded) = (terms(1), terms(2), terms(3));
    (l2p.direction, l2p.liquid_amount, l2p.polygon_amount) = (Direction::L2P, 1_000_000_00, 997_000);
    (p2l.direction, p2l.liquid_amount, p2l.polygon_amount) = (Direction::P2L, 995_000_00, 1_000_000);
    (refunded.direction, refunded.liquid_amount, refunded.polygon_amount) = (Direction::L2P, 500_000_00, 498_000);
    for t in [&l2p, &p2l, &refunded] {
        assert!(s.accept(t, &State::WatchLock).unwrap());
    }
    let done = State::Done { outcome: Outcome::Completed };
    s.commit_step(&l2p.swap_id(), &done, &[]).unwrap();
    s.commit_step(&p2l.swap_id(), &done, &[]).unwrap();
    s.commit_step(&refunded.swap_id(), &State::Done { outcome: Outcome::Refunded }, &[]).unwrap();
    s.record_cost(&l2p.swap_id(), "ClaimLiquid", 30, 0).unwrap();
    s.record_cost(&l2p.swap_id(), "ClaimLiquid", 30, 0).unwrap();
    s.record_cost(&l2p.swap_id(), "LockPolygon", 0, 3_000).unwrap();
    s.record_cost(&l2p.swap_id(), "LockPolygon", 0, 3_000).unwrap();
    let r = s.report().unwrap();
    let total = |d: &str, o: &str| r.swaps.iter().find(|((_, dir, out), _)| dir == d && out == o).map(|(_, l)| *l).unwrap();
    assert_eq!(total("L2P", "Completed"), liq2any_maker_core::store::ReportLine { swaps: 1, volume: 1_000_000, spread: 3_000 });
    assert_eq!(total("P2L", "Completed"), liq2any_maker_core::store::ReportLine { swaps: 1, volume: 1_000_000, spread: 5_000 });
    let refund = total("L2P", "Refunded");
    assert_eq!((refund.swaps, refund.spread), (1, 0), "a refund earns nothing");
    assert_eq!(r.costs.values().copied().fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1)), (30, 3_000), "a retry isn't a second cost");
}

#[test]
fn a_date_comes_out_right() {
    assert_eq!(liq2any_maker_core::store::day_of(0), "1970-01-01");
    assert_eq!(liq2any_maker_core::store::day_of(1_793_996_585), "2026-11-06");
    assert_eq!(liq2any_maker_core::store::day_of(951_782_400), "2000-02-29");
}
