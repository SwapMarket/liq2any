//! The Polygon form of the lock point, against the shared vectors that the Solidity tests also read
//! (`contracts/polygon/test/vectors.txt`, made by `scripts/polygon-vectors.py` from the curve's and keccak's definitions).

use liq2any_swapkey::{point_of, polygon_address_of, polygon_value, secret_from_polygon, solana_point};

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn polygon_vectors_match() {
    let file = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../polygon/test/vectors.txt")).unwrap();
    let mut count = 0;
    for line in file.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let (t, s, xy, address) = (hex32(fields[0]), hex32(fields[1]), fields[2], fields[3]);
        assert_eq!(hex(&polygon_value(&t).unwrap()), fields[1], "s");
        assert_eq!(secret_from_polygon(&s).unwrap(), t, "t back from s");
        assert_eq!(hex(&solana_point(&t).unwrap()), xy, "T");
        assert_eq!(hex(&polygon_address_of(&point_of(&t).unwrap()).unwrap()), address, "address(T)");
        count += 1;
    }
    assert_eq!(count, 8);
}

#[test]
fn private_key_one_has_its_well_known_address() {
    let mut one = [0u8; 32];
    one[31] = 1;
    let address = polygon_address_of(&point_of(&one).unwrap()).unwrap();
    assert_eq!(hex(&address), "7e5f4552091a69125d5dfcb7b8c2659029395bdf");
}

#[test]
fn a_point_that_is_not_on_the_curve_is_refused() {
    let mut bad = [0u8; 33];
    bad[0] = 2;
    bad[32] = 5; // x = 5 has no point on secp256k1
    assert!(polygon_address_of(&bad).is_err());
}
