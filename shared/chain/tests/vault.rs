//! The maker vault's side of `polygon.rs`: the window rule as `MakerVault.afterLock` has it, and the lock's calldata against
//! Foundry's `cast`, which is skipped, saying so, where `cast` isn't installed.

use std::process::Command;

use liq2any_chain::evm::{to_hex, Address};
use liq2any_chain::polygon::{vault_lock_call, VaultState};

fn state(start: u64, used: u128) -> VaultState {
    VaultState {
        escrow: Address([1; 20]),
        operator: Address([2; 20]),
        window_secs: 100,
        window_cap: 1_000,
        window_start: start,
        window_used: used,
    }
}

#[test]
fn room_follows_the_window() {
    assert_eq!(state(1_000, 400).room(1_000), 600, "the window's start");
    assert_eq!(state(1_000, 400).room(1_099), 600, "its last second");
    assert_eq!(state(1_000, 400).room(1_100), 1_000, "at its end, a whole cap again");
    assert_eq!(state(0, 0).room(5), 1_000, "a vault that has never locked");
    assert_eq!(state(1_000, 1_000).room(1_050), 0);
    assert_eq!(state(1_000, 1_500).room(1_050), 0, "a cap lowered below what was used leaves nothing, without underflow");
    assert_eq!(state(u64::MAX - 10, 1_000).room(u64::MAX), 0, "no overflow at the end of time");
}

#[test]
fn the_lock_call_matches_cast() {
    let Ok(out) = Command::new("cast")
        .args([
            "calldata",
            "lock(address,address,uint128,uint64,uint64,address)",
            "0x0101010101010101010101010101010101010101",
            "0x0202020202020202020202020202020202020202",
            "25000000",
            "1800000000",
            "54000000",
            "0x0303030303030303030303030303030303030303",
        ])
        .output()
    else {
        println!("skipped: no cast on the PATH");
        return;
    };
    let want = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let ours = vault_lock_call(&Address([1; 20]), &Address([2; 20]), 25_000_000, 1_800_000_000, 54_000_000, &Address([3; 20]));
    assert_eq!(format!("0x{}", to_hex(&ours)), want);
}
