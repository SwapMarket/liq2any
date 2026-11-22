//! Against the live escrow on Amoy, with transactions signed and sent by this crate's own code. TEST NETWORK ONLY.
//!
//!   AMOY_KEY_FILE=contracts/polygon/.keys/amoy-deployer.json cargo test -p liq2any-chain --test amoy -- --ignored --nocapture
//!
//! Spends a little test POL and stand-in tUSDT0 from that key.

use std::time::{Duration, Instant};

use liq2any_chain::evm::{address_of, calldata, from_hex, Address, Tx, Word};
use liq2any_chain::polygon::{self, approve_call, claim_call, lock_call, lock_id, vault_of, Rpc, State};
use liq2any_swapkey::{point_of, polygon_address_of, polygon_value};
use secp256k1::SecretKey;

const RPC: &str = "https://polygon-amoy-bor-rpc.publicnode.com";
const SECOND_RPC: &str = "https://polygon-amoy.drpc.org";
const ESCROW: &str = "0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F";
const TOKEN: &str = "0xb4236fbd3bc5791Ee0faB7f065f96F591d33a724";

fn key() -> SecretKey {
    let path = std::env::var("AMOY_KEY_FILE").expect("AMOY_KEY_FILE");
    let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    SecretKey::from_slice(&from_hex(file[0]["private_key"].as_str().unwrap()).unwrap()).unwrap()
}

fn send(rpc: &Rpc, key: &SecretKey, to: &Address, data: Vec<u8>, value: u128) -> polygon::Receipt {
    let from = address_of(key);
    let (base, priority) = rpc.fees().unwrap();
    let gas = rpc.estimate_gas(&from, to, &data, value).unwrap() * 13 / 10;
    let tx = Tx {
        chain_id: 80_002,
        nonce: rpc.nonce(&from, "pending").unwrap(),
        max_priority_fee_per_gas: priority.max(25_000_000_000),
        max_fee_per_gas: base * 2 + priority.max(25_000_000_000),
        gas,
        to: *to,
        value,
        data,
    };
    let hash = rpc.send_raw(&tx.sign(key).unwrap()).unwrap();
    let start = Instant::now();
    loop {
        if let Some(r) = rpc.receipt(&hash).unwrap() {
            return r;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "no receipt");
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[test]
#[ignore]
fn reads_and_writes_on_amoy() {
    let rpc = Rpc::new(RPC);
    let other = Rpc::new(SECOND_RPC);
    let (escrow, token) = (Address::parse(ESCROW).unwrap(), Address::parse(TOKEN).unwrap());
    assert_eq!(rpc.chain_id().unwrap(), 80_002);
    let key = key();
    let me = address_of(&key);

    // The vault address computed here is the contract's own.
    let implementation = rpc.vault_implementation(&escrow).unwrap();
    let id = [7u8; 32];
    let theirs = rpc.call_at(&escrow, &calldata("vaultOf(bytes32)", &[Word::Bytes32(id)]), "latest").unwrap();
    assert_eq!(vault_of(&escrow, &implementation, &id).0, theirs[12..32]);

    // A fresh secret for this run, from the time, so runs never reuse a lock id.
    let mut t = [0u8; 32];
    t[..16].copy_from_slice(&std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos().to_be_bytes());
    t[31] = 1;
    let lock_address = Address(polygon_address_of(&point_of(&t).unwrap()).unwrap());
    let s = polygon_value(&t).unwrap();
    let recipient = Address::parse("0x000000000000000000000000000000000000aa01").unwrap();
    let gas_to = Address::parse("0x000000000000000000000000000000000000aa02").unwrap();

    // Faucet, approve, lock with a gas drop, claim: all signed here.
    assert!(send(&rpc, &key, &token, calldata("faucet(uint256)", &[Word::Uint(5_000_000)]), 0).success);
    assert!(send(&rpc, &key, &token, approve_call(&escrow, 5_000_000), 0).success);
    let head = rpc.block("latest").unwrap();
    let before = rpc.token_balance(&token, &recipient, "latest").unwrap();
    let lock = lock_call(&lock_address, &recipient, 5_000_000, head.time + 3600, head.number + 3600, false, &gas_to);
    let receipt = send(&rpc, &key, &escrow, lock, 1_000_000_000_000);
    assert!(receipt.success);
    println!("lock: {} gas", receipt.gas_used);

    let id = lock_id(&me, &lock_address);
    let open = rpc.lock_at(&escrow, &id, "latest").unwrap();
    assert_eq!((open.state, open.recipient, open.amount, open.lock_address), (State::Open, recipient, 5_000_000, lock_address));
    assert!(!open.long_leg && open.gas_drop == 1_000_000_000_000);
    assert!(open.can_claim(head.time, head.number));
    assert_eq!(rpc.token_balance(&token, &vault_of(&escrow, &implementation, &id), "latest").unwrap(), 5_000_000);

    let receipt = send(&rpc, &key, &escrow, claim_call(&id, &s), 0);
    assert!(receipt.success);
    println!("claim: {} gas", receipt.gas_used);
    let claimed = rpc.lock_at(&escrow, &id, "latest").unwrap();
    assert_eq!((claimed.state, claimed.revealed), (State::Claimed, s));
    assert_eq!(rpc.token_balance(&token, &recipient, "latest").unwrap(), before + 5_000_000);
    assert!(!rpc.is_blocked(&token, &recipient, "latest").unwrap());

    // The second endpoint agrees once the claim is final.
    let start = Instant::now();
    loop {
        let f = other.block("finalized").unwrap();
        if f.number >= receipt.block {
            assert_eq!(other.lock_at(&escrow, &id, "finalized").unwrap().state, State::Claimed);
            println!("finalized on {} after {:?}", other.url(), start.elapsed());
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "not finalized");
        std::thread::sleep(Duration::from_secs(2));
    }
}
