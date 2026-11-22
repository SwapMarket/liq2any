//! The EVM pieces the maker needs on Polygon, with no I/O: addresses, keccak-256, ABI encoding for the escrow's few functions,
//! and signed EIP-1559 transactions. Small and hand-written instead of a large library (docs/design.md section 4.1), and checked
//! byte for byte against Foundry's `cast` in `tests/evm.rs`.

use secp256k1::ecdsa::RecoverableSignature;
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use sha3::{Digest, Keccak256};

pub type Res<T> = Result<T, String>;

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// A 20-byte account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Address(pub [u8; 20]);

impl Address {
    pub const ZERO: Address = Address([0; 20]);

    /// The address of a public key: the last 20 bytes of keccak-256 of its uncompressed x and y.
    pub fn of_key(key: &PublicKey) -> Self {
        let uncompressed = key.serialize_uncompressed();
        let hash = keccak256(&uncompressed[1..]);
        let mut out = [0u8; 20];
        out.copy_from_slice(&hash[12..]);
        Address(out)
    }

    /// Parses `0x` and 40 hex digits. A mixed-case address must carry a correct EIP-55 checksum; all-lower or all-upper
    /// has none to check.
    pub fn parse(text: &str) -> Res<Self> {
        let hex = text.strip_prefix("0x").ok_or_else(|| format!("{text:?} is not a 0x address"))?;
        let bytes = from_hex(hex)?;
        let bytes: [u8; 20] = bytes.try_into().map_err(|_| format!("{text:?} is not 20 bytes"))?;
        let address = Address(bytes);
        let has_lower = hex.bytes().any(|c| c.is_ascii_lowercase());
        let has_upper = hex.bytes().any(|c| c.is_ascii_uppercase());
        if has_lower && has_upper && address.checksummed() != text {
            return Err(format!("{text:?} has a wrong EIP-55 checksum"));
        }
        Ok(address)
    }

    /// EIP-55 mixed-case form.
    pub fn checksummed(&self) -> String {
        let lower = to_hex(&self.0);
        let hash = keccak256(lower.as_bytes());
        let mut out = String::from("0x");
        for (i, c) in lower.chars().enumerate() {
            let nibble = (hash[i / 2] >> if i % 2 == 0 { 4 } else { 0 }) & 0xf;
            out.push(if c.is_ascii_alphabetic() && nibble >= 8 { c.to_ascii_uppercase() } else { c });
        }
        out
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.checksummed())
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Res<Vec<u8>> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if text.len() % 2 != 0 || !text.is_ascii() {
        return Err(format!("{text:?} is not hex"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| format!("{text:?} is not hex")))
        .collect()
}

// ---- ABI: the static types the escrow uses ----

/// One ABI argument. Every one the escrow takes is a single 32-byte word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    Address(Address),
    Uint(u128),
    Bool(bool),
    Bytes32([u8; 32]),
}

impl Word {
    fn encode(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        match self {
            Word::Address(a) => out[12..].copy_from_slice(&a.0),
            Word::Uint(v) => out[16..].copy_from_slice(&v.to_be_bytes()),
            Word::Bool(b) => out[31] = u8::from(*b),
            Word::Bytes32(b) => out = *b,
        }
        out
    }
}

/// The 4-byte selector of a signature such as `claim(bytes32,bytes32)`.
pub fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

/// Calldata for a function of static arguments.
pub fn calldata(signature: &str, args: &[Word]) -> Vec<u8> {
    let mut out = selector(signature).to_vec();
    for arg in args {
        out.extend_from_slice(&arg.encode());
    }
    out
}

/// The `n`th 32-byte word of returned data.
pub fn word(data: &[u8], n: usize) -> Res<[u8; 32]> {
    data.get(n * 32..n * 32 + 32)
        .map(|w| w.try_into().expect("32 bytes"))
        .ok_or_else(|| format!("returned data has no word {n}"))
}

/// A word as an unsigned integer that must fit 128 bits.
pub fn word_u128(data: &[u8], n: usize) -> Res<u128> {
    let w = word(data, n)?;
    if w[..16].iter().any(|&b| b != 0) {
        return Err(format!("word {n} is over 128 bits"));
    }
    Ok(u128::from_be_bytes(w[16..].try_into().expect("16 bytes")))
}

/// A word as an address: its top 12 bytes must be zero.
pub fn word_address(data: &[u8], n: usize) -> Res<Address> {
    let w = word(data, n)?;
    if w[..12].iter().any(|&b| b != 0) {
        return Err(format!("word {n} is not an address"));
    }
    Ok(Address(w[12..].try_into().expect("20 bytes")))
}

pub fn word_bool(data: &[u8], n: usize) -> Res<bool> {
    match word_u128(data, n)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("word {n} is not a bool")),
    }
}

// ---- RLP ----

fn rlp_length(len: usize, short: u8, out: &mut Vec<u8>) {
    if len <= 55 {
        out.push(short + len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first = bytes.iter().position(|&b| b != 0).expect("len > 55");
        out.push(short + 55 + (bytes.len() - first) as u8);
        out.extend_from_slice(&bytes[first..]);
    }
}

pub fn rlp_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        out.push(bytes[0]);
    } else {
        rlp_length(bytes.len(), 0x80, out);
        out.extend_from_slice(bytes);
    }
}

/// An unsigned integer: big-endian with no leading zeros, and zero as the empty string.
pub fn rlp_uint(value: u128, out: &mut Vec<u8>) {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    rlp_bytes(&bytes[first..], out);
}

pub fn rlp_list(items: &[u8], out: &mut Vec<u8>) {
    rlp_length(items.len(), 0xc0, out);
    out.extend_from_slice(items);
}

// ---- Transactions ----

/// An EIP-1559 (type 2) transaction with an empty access list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas: u64,
    pub to: Address,
    pub value: u128,
    pub data: Vec<u8>,
}

impl Tx {
    fn fields(&self) -> Vec<u8> {
        let mut f = Vec::new();
        rlp_uint(u128::from(self.chain_id), &mut f);
        rlp_uint(u128::from(self.nonce), &mut f);
        rlp_uint(self.max_priority_fee_per_gas, &mut f);
        rlp_uint(self.max_fee_per_gas, &mut f);
        rlp_uint(u128::from(self.gas), &mut f);
        rlp_bytes(&self.to.0, &mut f);
        rlp_uint(self.value, &mut f);
        rlp_bytes(&self.data, &mut f);
        rlp_list(&[], &mut f);
        f
    }

    /// What is signed: keccak-256 of `0x02 ‖ rlp([chain_id, nonce, …, access_list])`.
    pub fn signing_hash(&self) -> [u8; 32] {
        let mut out = vec![0x02];
        rlp_list(&self.fields(), &mut out);
        keccak256(&out)
    }

    /// The raw signed transaction, `0x02 ‖ rlp([…, y_parity, r, s])`, ready for `eth_sendRawTransaction`. libsecp256k1
    /// signs with RFC 6979 nonces and low `s`, so the same key and fields always give the same bytes.
    pub fn sign(&self, key: &SecretKey) -> Res<Vec<u8>> {
        let secp = Secp256k1::signing_only();
        let msg = Message::from_digest(self.signing_hash());
        let sig: RecoverableSignature = secp.sign_ecdsa_recoverable(&msg, key);
        let (recovery, compact) = sig.serialize_compact();
        let parity = recovery.to_i32();
        if !(0..=1).contains(&parity) {
            return Err(format!("unexpected recovery id {parity}"));
        }
        let mut f = self.fields();
        rlp_uint(parity as u128, &mut f);
        rlp_bytes(strip_zeros(&compact[..32]), &mut f);
        rlp_bytes(strip_zeros(&compact[32..]), &mut f);
        let mut out = vec![0x02];
        rlp_list(&f, &mut out);
        Ok(out)
    }
}

/// The transaction hash of a raw signed transaction.
pub fn tx_hash(raw: &[u8]) -> [u8; 32] {
    keccak256(raw)
}

fn strip_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

/// The address a key controls.
pub fn address_of(key: &SecretKey) -> Address {
    Address::of_key(&PublicKey::from_secret_key(&Secp256k1::signing_only(), key))
}
