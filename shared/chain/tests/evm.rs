//! `evm.rs` against Foundry's `cast`, byte for byte, from `tests/evm-vectors.txt` (made by `scripts/evm-vectors.sh`).

use liq2any_chain::evm::{address_of, calldata, decode_signed, from_hex, to_hex, tx_hash, Address, Tx, Word};
use secp256k1::SecretKey;

fn key(text: &str) -> SecretKey {
    SecretKey::from_slice(&from_hex(text).unwrap()).unwrap()
}

fn words(signature: &str, args: &str) -> Vec<Word> {
    let types = &signature[signature.find('(').unwrap() + 1..signature.len() - 1];
    types
        .split(',')
        .zip(args.split(','))
        .map(|(ty, arg)| match ty {
            "address" => Word::Address(Address::parse(arg).unwrap()),
            "bool" => Word::Bool(arg == "true"),
            "bytes32" => Word::Bytes32(from_hex(arg).unwrap().try_into().unwrap()),
            t if t.starts_with("uint") => Word::Uint(arg.parse().unwrap()),
            t => panic!("type {t}"),
        })
        .collect()
}

#[test]
fn matches_cast() {
    let file = include_str!("evm-vectors.txt");
    let (mut addresses, mut checksums, mut calls, mut txs) = (0, 0, 0, 0);
    let mut signing_key = None;
    for line in file.lines() {
        let f: Vec<&str> = line.split('|').collect();
        match f[0] {
            "address" => {
                signing_key = Some(key(f[1]));
                assert_eq!(address_of(&key(f[1])).checksummed(), f[2]);
                addresses += 1;
            }
            "checksum" => {
                let a = Address::parse(f[1]).unwrap();
                assert_eq!(a.checksummed(), f[2]);
                assert_eq!(Address::parse(f[2]).unwrap(), a, "a checksummed address parses");
                checksums += 1;
            }
            "calldata" => {
                assert_eq!(format!("0x{}", to_hex(&calldata(f[1], &words(f[1], f[2])))), f[3], "{}", f[1]);
                calls += 1;
            }
            "tx" => {
                let tx = Tx {
                    chain_id: f[1].parse().unwrap(),
                    nonce: f[2].parse().unwrap(),
                    max_priority_fee_per_gas: f[3].parse().unwrap(),
                    max_fee_per_gas: f[4].parse().unwrap(),
                    gas: f[5].parse().unwrap(),
                    to: Address::parse(f[6]).unwrap(),
                    value: f[7].parse().unwrap(),
                    data: if f[8].is_empty() { vec![] } else { from_hex(f[8]).unwrap() },
                };
                let raw = tx.sign(signing_key.as_ref().unwrap()).unwrap();
                assert_eq!(format!("0x{}", to_hex(&raw)), f[9], "tx line {line}");
                assert_eq!(tx_hash(&raw).len(), 32);
                assert_eq!(decode_signed(&raw).unwrap(), tx, "decoding gives back what was signed");
                txs += 1;
            }
            other => panic!("unknown kind {other}"),
        }
    }
    assert_eq!((addresses, checksums, calls, txs), (1, 3, 5, 5));
}

#[test]
fn a_wrong_checksum_is_refused() {
    assert!(Address::parse("0xc2132D05D31c914a87C6611C10748AEb04B58e8F").is_ok());
    // One letter's case flipped.
    assert!(Address::parse("0xC2132D05D31c914a87C6611C10748AEb04B58e8F").is_err());
    assert!(Address::parse("0xc2132d05d31c914a87c6611c10748aeb04b58e8").is_err(), "39 digits");
    assert!(Address::parse("c2132d05d31c914a87c6611c10748aeb04b58e8f").is_err(), "no 0x");
    assert!(Address::parse("0xg2132d05d31c914a87c6611c10748aeb04b58e8f").is_err(), "not hex");
}

#[test]
fn returned_words_are_checked() {
    use liq2any_chain::evm::{word_address, word_bool, word_u128};
    let mut data = vec![0u8; 96];
    data[31] = 1;
    data[63] = 2;
    assert!(word_bool(&data, 0).unwrap());
    assert!(word_bool(&data, 1).is_err(), "2 is no bool");
    assert_eq!(word_u128(&data, 1).unwrap(), 2);
    data[64] = 1;
    assert!(word_address(&data, 2).is_err(), "dirty top bytes");
    assert!(word_u128(&data, 2).is_err(), "over 128 bits");
    assert!(word_u128(&data, 3).is_err(), "past the end");
}

#[test]
fn decoding_refuses_anything_but_our_shape() {
    let key = SecretKey::from_slice(&[0x11; 32]).unwrap();
    let tx = Tx {
        chain_id: 137,
        nonce: 3,
        max_priority_fee_per_gas: 30_000_000_000,
        max_fee_per_gas: 600_000_000_000,
        gas: 250_000,
        to: Address([0xaa; 20]),
        value: 0,
        data: vec![1, 2, 3],
    };
    let raw = tx.sign(&key).unwrap();
    assert_eq!(decode_signed(&raw).unwrap(), tx);
    assert!(decode_signed(&raw[1..]).is_err(), "no type byte");
    assert!(decode_signed(&raw[..raw.len() - 1]).is_err(), "truncated");
    let mut extra = raw.clone();
    extra.push(0);
    assert!(decode_signed(&extra).is_err(), "trailing bytes");
    let mut legacy = raw.clone();
    legacy[0] = 0x01;
    assert!(decode_signed(&legacy).is_err(), "another type");
    // Every single flipped byte either fails or decodes to something other than the original.
    for i in 1..raw.len() {
        let mut bad = raw.clone();
        bad[i] ^= 0x80;
        if let Ok(other) = decode_signed(&bad) {
            assert!(other != tx || bad[i..].len() <= 66, "byte {i}");
        }
    }
}
