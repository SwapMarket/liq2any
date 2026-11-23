//! The browser's Polygon calls against an encoder that isn't ours: Foundry's `cast`, and the escrow's own `idOf` through
//! `cast keccak`. The page calls the maker's shared code, so this checks that the arguments reach it in the right places.
//! Skipped, saying so, where `cast` isn't installed.

use std::process::Command;

use liq2any_client_wasm::*;

const ESCROW: &str = "0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F";
const MAKER: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
const TAKER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const ID: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

fn cast(args: &[&str]) -> Option<String> {
    let out = Command::new("cast").args(args).output().ok()?;
    assert!(out.status.success(), "cast {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A lock point from a known secret, and its address as the escrow sees it.
fn point() -> (String, String) {
    let secrets: serde_json::Value = serde_json::from_str(&new_swap_secrets().unwrap()).unwrap();
    let point = secrets["lock_point"].as_str().unwrap().to_string();
    let address = secrets["lock_address"].as_str().unwrap().to_string();
    assert_eq!(lock_address(&point).unwrap(), address, "the same address, however it is asked for");
    (point, address)
}

#[test]
fn calls_match_cast() {
    let Some(_) = cast(&["--version"]) else {
        println!("skipped: no cast on the PATH");
        return;
    };
    let (_, address) = point();
    let lock = build_lock_call(&address, MAKER, "25000000", 1_800_000_000, 54_000_000).unwrap();
    let want = cast(&[
        "calldata",
        "lock(address,address,uint128,uint64,uint64,bool,address)",
        &address,
        MAKER,
        "25000000",
        "1800000000",
        "54000000",
        "true",
        "0x0000000000000000000000000000000000000000",
    ])
    .unwrap();
    assert_eq!(lock, want, "a P→L lock: a long leg, with no gas drop");

    let revealed = "22".repeat(32);
    let claim = build_claim_call(ID, &revealed).unwrap();
    assert_eq!(claim, cast(&["calldata", "claim(bytes32,bytes32)", ID, &format!("0x{revealed}")]).unwrap());
    assert_eq!(build_refund_call(ID).unwrap(), cast(&["calldata", "refund(bytes32)", ID]).unwrap());
    let approve = build_approve_call(ESCROW, "340282366920938463463374607431768211455").unwrap();
    let want = cast(&["calldata", "approve(address,uint256)", ESCROW, "340282366920938463463374607431768211455"]).unwrap();
    assert_eq!(approve, want);
    assert_eq!(locks_call(ID).unwrap(), cast(&["calldata", "locks(bytes32)", ID]).unwrap());
    assert_eq!(token_call("balance", TAKER, "").unwrap(), cast(&["calldata", "balanceOf(address)", TAKER]).unwrap());
    assert_eq!(token_call("blocked", TAKER, "").unwrap(), cast(&["calldata", "isBlocked(address)", TAKER]).unwrap());
    let allowance = cast(&["calldata", "allowance(address,address)", TAKER, ESCROW]).unwrap();
    assert_eq!(token_call("allowance", TAKER, ESCROW).unwrap(), allowance);
    assert!(token_call("transfer", TAKER, "").is_err(), "only the reads the page needs");
    assert_eq!(build_faucet_call("100000000").unwrap(), cast(&["calldata", "faucet(uint256)", "100000000"]).unwrap());

    // idOf(initiator, lockAddress) = keccak256(abi.encode(initiator, lockAddress)).
    let encoded = cast(&["abi-encode", "f(address,address)", MAKER, &address]).unwrap();
    assert_eq!(lock_id(MAKER, &address).unwrap(), cast(&["keccak", &encoded]).unwrap());
}

#[test]
fn a_lock_reads_back() {
    // Eleven words, as `locks(id)` returns them, for an open short-leg lock with a gas drop.
    let (_, address) = point();
    let word = |hex: &str| format!("{hex:0>64}");
    let address_word = |a: &str| word(&a[2..].to_lowercase());
    let result = format!(
        "0x{}{}{}{}{}{}{}{}{}{}{}",
        address_word(MAKER),
        word("6b49d200"), // 1,800,000,000
        word("1"),
        word("0"),
        word("0"),
        address_word(TAKER),
        word("337f980"), // 54,000,000
        address_word(&address),
        word("38d7ea4c68000"), // 0.001 POL
        word("17d7840"),       // 25,000,000
        "33".repeat(32),
    );
    let lock: serde_json::Value = serde_json::from_str(&parse_lock(&result).unwrap()).unwrap();
    assert_eq!(lock["initiator"], MAKER);
    assert_eq!(lock["recipient"], TAKER);
    assert_eq!(lock["lock_address"], address.as_str());
    assert_eq!(lock["state"], "open");
    assert_eq!(lock["long_leg"], false);
    assert_eq!(lock["deadline_time"], 1_800_000_000u64);
    assert_eq!(lock["deadline_block"], 54_000_000u64);
    assert_eq!(lock["amount"], "25000000");
    assert_eq!(lock["gas_drop"], "1000000000000000");
    assert_eq!(lock["revealed"], "33".repeat(32));
    assert!(parse_lock("0x00").is_err(), "a short answer is refused, not read as zeros");
}

#[test]
fn polygon_addresses_are_checked() {
    assert_eq!(polygon_address_problem(TAKER), "");
    assert_eq!(polygon_address_problem(&format!(" {TAKER}\n")), "", "spaces around a pasted address don't matter");
    assert_eq!(polygon_address_problem(&TAKER.to_lowercase()), "", "all lower case has no checksum to check");
    let mut bad = TAKER.to_string();
    bad.replace_range(41..42, "c");
    assert!(polygon_address_problem(&bad).contains("checksum"), "{bad}");
    assert_eq!(polygon_address_problem("0x0000000000000000000000000000000000000000"), "That is the zero address, which nobody can spend from.");
    assert_eq!(polygon_address_problem("tlq1qq"), "This isn't a Polygon address.");
    assert_eq!(checksummed(&TAKER.to_lowercase()).unwrap(), TAKER);
}
