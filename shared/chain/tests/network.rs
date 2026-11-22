//! Choosing a network: what the operator says must match what both chains say, in both directions.
use std::str::FromStr;

use liq2any_chain::evm::Address;
use liq2any_chain::liquid::{
    check_genesis, Net, NetworkConfig, LIQUID_MAINNET_GENESIS, LIQUID_TESTNET_GENESIS, MAINNET_LBTC, MAINNET_MAX_PER_SWAP,
    MAINNET_USDT, POLYGON_MAINNET_CHAIN_ID,
};
use liq2any_chain::polygon;
use simplicityhl::elements::{AddressParams, AssetId};

const AMOY: u64 = 80_002;

fn amoy_ids() -> Option<(Address, Address)> {
    Some((
        Address::parse("0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F").unwrap(),
        Address::parse("0xb4236fbd3bc5791Ee0faB7f065f96F591d33a724").unwrap(),
    ))
}

#[test]
fn a_network_must_match_both_chains() {
    assert!(check_genesis(Net::Testnet, LIQUID_TESTNET_GENESIS, AMOY).is_ok());
    assert!(check_genesis(Net::Testnet, "a regtest genesis", 31_337).is_ok());
    assert!(check_genesis(Net::Mainnet, LIQUID_MAINNET_GENESIS, POLYGON_MAINNET_CHAIN_ID).is_ok());
    // Real funds under test rules: either chain on mainnet is enough to stop.
    assert!(check_genesis(Net::Testnet, LIQUID_MAINNET_GENESIS, AMOY).is_err());
    assert!(check_genesis(Net::Testnet, LIQUID_TESTNET_GENESIS, POLYGON_MAINNET_CHAIN_ID).is_err());
    assert!(check_genesis(Net::Testnet, LIQUID_MAINNET_GENESIS, POLYGON_MAINNET_CHAIN_ID).is_err());
    // A mixed pair could never settle: one leg would be worthless.
    assert!(check_genesis(Net::Mainnet, LIQUID_MAINNET_GENESIS, AMOY).is_err());
    assert!(check_genesis(Net::Mainnet, LIQUID_TESTNET_GENESIS, POLYGON_MAINNET_CHAIN_ID).is_err());
    assert!(check_genesis(Net::Mainnet, LIQUID_TESTNET_GENESIS, AMOY).is_err());
}

#[test]
fn network_names_are_exact() {
    assert_eq!(Net::parse("testnet"), Ok(Net::Testnet));
    assert_eq!(Net::parse("mainnet"), Ok(Net::Mainnet));
    assert_eq!(Net::default(), Net::Testnet, "nothing reaches mainnet by leaving a setting out");
    for bad in ["", "Mainnet", "main", "mainnet ", "liquidv1", "polygon"] {
        assert!(Net::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn mainnet_constants_are_the_real_ones() {
    let main = NetworkConfig::mainnet();
    assert_eq!(main.lbtc, AssetId::LIQUID_BTC, "the elements crate's own constant");
    assert_eq!(MAINNET_LBTC, AssetId::LIQUID_BTC.to_string());
    assert_eq!(main.usdt, AssetId::from_str(MAINNET_USDT).unwrap());
    assert_eq!(*main.params, AddressParams::LIQUID);
    assert_eq!(MAINNET_MAX_PER_SWAP, 1_000_000_000, "1000 USDT, in units of 10^-6");
    assert_eq!(POLYGON_MAINNET_CHAIN_ID, 137);
    assert_eq!(polygon::MAINNET_TOKEN.to_string(), "0xc2132D05D31c914a87C6611C10748AEb04B58e8F");
    assert_eq!(polygon::MAINNET_ESCROW, None, "not deployed yet");
}

/// One process, so these run in order inside one test: the first successful selection fixes the network for good.
#[test]
fn selecting_a_network() {
    let (l, p) = (LIQUID_MAINNET_GENESIS, POLYGON_MAINNET_CHAIN_ID);
    // Mainnet refuses test settings, test ids and limits over the cap, before anything is chosen.
    let over = polygon::enter("mainnet", false, &[50_000_000, MAINNET_MAX_PER_SWAP + 1], l, p, None).unwrap_err();
    assert!(over.contains("above mainnet's cap"), "{over}");
    assert!(polygon::enter("mainnet", true, &[1], l, p, None).unwrap_err().contains("regtest_assets"));
    assert!(polygon::enter("Mainnet", false, &[1], l, p, None).unwrap_err().contains("unknown network"));
    assert!(polygon::enter("mainnet", false, &[1], l, p, amoy_ids()).unwrap_err().contains("test escrow"));
    // At the cap exactly, the only thing left in the way is the escrow not being deployed.
    assert!(polygon::enter("mainnet", false, &[MAINNET_MAX_PER_SWAP], l, p, None).unwrap_err().contains("isn't on Polygon mainnet yet"));
    // A test network needs its ids, and never mainnet's token.
    assert!(polygon::enter("testnet", false, &[1], LIQUID_TESTNET_GENESIS, AMOY, None).unwrap_err().contains("configured"));
    let fake = Some((Address([1; 20]), polygon::MAINNET_TOKEN));
    assert!(polygon::enter("testnet", false, &[1], LIQUID_TESTNET_GENESIS, AMOY, fake).unwrap_err().contains("mainnet's USDT0"));
    assert!(polygon::enter("testnet", false, &[1], LIQUID_TESTNET_GENESIS, p, amoy_ids()).is_err(), "Polygon mainnet under testnet");
    // Testnet keeps whatever limits its operator sets.
    assert_eq!(polygon::enter("testnet", true, &[u64::MAX], LIQUID_TESTNET_GENESIS, AMOY, amoy_ids()), Ok(Net::Testnet));
    assert_eq!(polygon::ids().chain_id, AMOY);
    // Once chosen, nothing in the process can switch.
    let other = Some((Address([2; 20]), Address([3; 20])));
    assert!(polygon::select(Net::Testnet, LIQUID_TESTNET_GENESIS, AMOY, other).unwrap_err().contains("already chose"));
    assert!(polygon::select(Net::Testnet, LIQUID_TESTNET_GENESIS, 31_337, amoy_ids()).unwrap_err().contains("already chose"));
    assert_eq!(polygon::ids().escrow, amoy_ids().unwrap().0);
}
