//! Polygon side: choosing the network once, a small JSON-RPC client, typed reads of the escrow and the token, and the calls
//! the maker sends. Specification: docs/design.md sections 2 and 4.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::evm::{calldata, from_hex, keccak256, to_hex, word, word_address, word_bool, word_u128, Address, Word};
use crate::liquid::{check_genesis, use_network, Net, NetworkConfig, POLYGON_MAINNET_CHAIN_ID};

pub type Res<T> = Result<T, String>;

/// Tether's USDT0 on Polygon PoS mainnet, 6 decimals (docs/assumptions.md section 1.1).
pub const MAINNET_TOKEN: Address = Address([
    0xc2, 0x13, 0x2d, 0x05, 0xd3, 0x1c, 0x91, 0x4a, 0x87, 0xc6, 0x61, 0x1c, 0x10, 0x74, 0x8a, 0xeb, 0x04, 0xb5, 0x8e, 0x8f,
]);
/// The escrow on Polygon mainnet. None until the user deploys it, immutable, from a release (decision P3).
pub const MAINNET_ESCROW: Option<Address> = None;

/// The escrow and token this process deals with, and the chain it expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub chain_id: u64,
    pub escrow: Address,
    pub token: Address,
}

static IDS: OnceLock<Ids> = OnceLock::new();

/// The ids chosen by `select`. Panics if none were chosen, because nothing on Polygon may happen before that.
pub fn ids() -> Ids {
    *IDS.get().expect("polygon::select must run before anything touches Polygon")
}

/// What every program does at startup: read the network it was told, hold both chains to it, and on mainnet refuse test
/// settings and any per-swap limit above the release's cap. `caps` are the per-swap limits the caller will apply.
/// `test_ids` names the escrow and token on a test network, from the configuration; mainnet's are pinned in this file.
pub fn enter(
    network: &str,
    regtest: bool,
    caps: &[u64],
    liquid_genesis: &str,
    chain_id: u64,
    test_ids: Option<(Address, Address)>,
) -> Res<Net> {
    let net = Net::parse(network)?;
    if net == Net::Mainnet {
        if regtest {
            return Err("refusing to start: regtest_assets can't be set on mainnet".into());
        }
        if let Some(over) = caps.iter().find(|&&cap| cap > crate::liquid::MAINNET_MAX_PER_SWAP) {
            return Err(format!(
                "refusing to start: a per-swap limit of {over} is above mainnet's cap of {}",
                crate::liquid::MAINNET_MAX_PER_SWAP
            ));
        }
    }
    select(net, liquid_genesis, chain_id, test_ids)?;
    Ok(net)
}

/// Settle which network this process works on, once, before any address is derived or key loaded.
pub fn select(net: Net, liquid_genesis: &str, chain_id: u64, test_ids: Option<(Address, Address)>) -> Res<()> {
    check_genesis(net, liquid_genesis, chain_id)?;
    let wanted = match net {
        Net::Mainnet => {
            if test_ids.is_some() {
                return Err("refusing to start: a test escrow or token is configured on mainnet".into());
            }
            let escrow = MAINNET_ESCROW.ok_or("refusing to start: the escrow isn't on Polygon mainnet yet")?;
            Ids { chain_id: POLYGON_MAINNET_CHAIN_ID, escrow, token: MAINNET_TOKEN }
        }
        Net::Testnet => {
            let (escrow, token) = test_ids.ok_or("refusing to start: a test network needs its escrow and token configured")?;
            if token == MAINNET_TOKEN {
                return Err("refusing to start: mainnet's USDT0 is configured on a test network".into());
            }
            Ids { chain_id, escrow, token }
        }
    };
    if *IDS.get_or_init(|| wanted) != wanted {
        return Err("refusing to start: this process already chose another network".into());
    }
    if net == Net::Mainnet {
        use_network(NetworkConfig::mainnet());
        if crate::liquid::usdt().to_string() != crate::liquid::MAINNET_USDT {
            return Err("refusing to start: this process already chose another network".into());
        }
    }
    Ok(())
}

// ---- The escrow ----

/// `Escrow.State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    None,
    Open,
    Claimed,
    Refunded,
}

/// One lock, as `locks(id)` returns it (docs/design.md section 2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub initiator: Address,
    pub deadline_time: u64,
    pub state: State,
    pub long_leg: bool,
    pub extended: bool,
    pub recipient: Address,
    pub deadline_block: u64,
    pub lock_address: Address,
    pub gas_drop: u128,
    pub amount: u128,
    pub revealed: [u8; 32],
}

impl Lock {
    pub fn decode(data: &[u8]) -> Res<Self> {
        if data.len() != 11 * 32 {
            return Err(format!("locks() returned {} bytes, not 352", data.len()));
        }
        let small = |n: usize| -> Res<u64> {
            u64::try_from(word_u128(data, n)?).map_err(|_| format!("word {n} is over 64 bits"))
        };
        let state = match word_u128(data, 2)? {
            0 => State::None,
            1 => State::Open,
            2 => State::Claimed,
            3 => State::Refunded,
            other => return Err(format!("unknown escrow state {other}")),
        };
        Ok(Lock {
            initiator: word_address(data, 0)?,
            deadline_time: small(1)?,
            state,
            long_leg: word_bool(data, 3)?,
            extended: word_bool(data, 4)?,
            recipient: word_address(data, 5)?,
            deadline_block: small(6)?,
            lock_address: word_address(data, 7)?,
            gas_drop: word_u128(data, 8)?,
            amount: word_u128(data, 9)?,
            revealed: word(data, 10)?,
        })
    }

    /// The contract's clock rule (docs/design.md section 2.4), for a block at `time` and `number`.
    pub fn can_claim(&self, time: u64, number: u64) -> bool {
        let time_open = time < self.deadline_time;
        let blocks_open = number < self.deadline_block;
        self.state == State::Open && if self.long_leg { time_open || blocks_open } else { time_open && blocks_open }
    }
}

/// `idOf(initiator, lockAddress)`: keccak-256 of the two, ABI-encoded.
pub fn lock_id(initiator: &Address, lock_address: &Address) -> [u8; 32] {
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(&initiator.0);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(&lock_address.0);
    keccak256(&data)
}

/// The vault holding a lock's coins: CREATE2 by the escrow, salted with the id, of the EIP-1167 proxy for `implementation`.
pub fn vault_of(escrow: &Address, implementation: &Address, id: &[u8; 32]) -> Address {
    let mut code = from_hex("3d602d80600a3d3981f3363d3d373d3d3d363d73").expect("constant");
    code.extend_from_slice(&implementation.0);
    code.extend_from_slice(&from_hex("5af43d82803e903d91602b57fd5bf3").expect("constant"));
    let mut preimage = vec![0xff];
    preimage.extend_from_slice(&escrow.0);
    preimage.extend_from_slice(id);
    preimage.extend_from_slice(&keccak256(&code));
    Address(keccak256(&preimage)[12..].try_into().expect("20 bytes"))
}

pub const LOCK: &str = "lock(address,address,uint128,uint64,uint64,bool,address)";
pub const CLAIM: &str = "claim(bytes32,bytes32)";
pub const REFUND: &str = "refund(bytes32)";
pub const RELEASE: &str = "release(bytes32)";
pub const EXTEND: &str = "extend(bytes32,uint64,uint64)";

#[allow(clippy::too_many_arguments)]
pub fn lock_call(
    lock_address: &Address,
    recipient: &Address,
    amount: u128,
    deadline_time: u64,
    deadline_block: u64,
    long_leg: bool,
    gas_to: &Address,
) -> Vec<u8> {
    calldata(
        LOCK,
        &[
            Word::Address(*lock_address),
            Word::Address(*recipient),
            Word::Uint(amount),
            Word::Uint(u128::from(deadline_time)),
            Word::Uint(u128::from(deadline_block)),
            Word::Bool(long_leg),
            Word::Address(*gas_to),
        ],
    )
}

pub fn claim_call(id: &[u8; 32], revealed: &[u8; 32]) -> Vec<u8> {
    calldata(CLAIM, &[Word::Bytes32(*id), Word::Bytes32(*revealed)])
}

pub fn refund_call(id: &[u8; 32]) -> Vec<u8> {
    calldata(REFUND, &[Word::Bytes32(*id)])
}

pub fn release_call(id: &[u8; 32]) -> Vec<u8> {
    calldata(RELEASE, &[Word::Bytes32(*id)])
}

pub fn extend_call(id: &[u8; 32], new_time: u64, new_block: u64) -> Vec<u8> {
    calldata(EXTEND, &[Word::Bytes32(*id), Word::Uint(u128::from(new_time)), Word::Uint(u128::from(new_block))])
}

pub fn approve_call(spender: &Address, amount: u128) -> Vec<u8> {
    calldata("approve(address,uint256)", &[Word::Address(*spender), Word::Uint(amount)])
}

// ---- JSON-RPC ----

/// A block as `eth_getBlockByNumber` describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub number: u64,
    pub time: u64,
    pub hash: [u8; 32],
}

/// The outcome of a sent transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub success: bool,
    pub block: u64,
    pub gas_used: u64,
}

/// One Polygon JSON-RPC endpoint. Decisions use `finalized` (docs/design.md section 3.4); the daemon cross-checks endpoints.
#[cfg(feature = "native")]
pub struct Rpc {
    url: String,
    agent: ureq::Agent,
}

#[cfg(feature = "native")]
fn quantity(value: &Value) -> Res<u128> {
    let text = value.as_str().ok_or_else(|| format!("{value} is not a quantity"))?;
    let hex = text.strip_prefix("0x").ok_or_else(|| format!("{text:?} is not a quantity"))?;
    u128::from_str_radix(if hex.is_empty() { "0" } else { hex }, 16).map_err(|e| format!("{text:?}: {e}"))
}

#[cfg(feature = "native")]
fn quantity_u64(value: &Value) -> Res<u64> {
    u64::try_from(quantity(value)?).map_err(|_| format!("{value} is over 64 bits"))
}

#[cfg(feature = "native")]
impl Rpc {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
        Self { url: url.to_string(), agent }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn call(&self, method: &str, params: Value) -> Res<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let text = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(|e| format!("{method} at {}: {e}", self.url))?
            .into_string()
            .map_err(|e| format!("{method} at {}: {e}", self.url))?;
        let reply: Value = serde_json::from_str(&text).map_err(|e| format!("{method} at {}: {e}", self.url))?;
        if let Some(error) = reply.get("error") {
            return Err(format!("{method} at {}: {error}", self.url));
        }
        reply.get("result").cloned().ok_or_else(|| format!("{method} at {}: no result", self.url))
    }

    pub fn chain_id(&self) -> Res<u64> {
        quantity_u64(&self.call("eth_chainId", json!([]))?)
    }

    /// `tag` is `finalized`, `latest`, or a block number.
    pub fn block(&self, tag: &str) -> Res<Block> {
        let b = self.call("eth_getBlockByNumber", json!([tag, false]))?;
        if b.is_null() {
            return Err(format!("no block {tag} at {}", self.url));
        }
        let hash = from_hex(b["hash"].as_str().unwrap_or_default())?;
        Ok(Block {
            number: quantity_u64(&b["number"])?,
            time: quantity_u64(&b["timestamp"])?,
            hash: hash.try_into().map_err(|_| "a block hash isn't 32 bytes".to_string())?,
        })
    }

    pub fn block_number_at(&self, number: u64) -> Res<Block> {
        self.block(&format!("0x{number:x}"))
    }

    /// `eth_call` at `tag`, returning the raw result.
    pub fn call_at(&self, to: &Address, data: &[u8], tag: &str) -> Res<Vec<u8>> {
        let result = self.call("eth_call", json!([{ "to": to.to_string(), "data": format!("0x{}", to_hex(data)) }, tag]))?;
        from_hex(result.as_str().ok_or("eth_call returned no data")?)
    }

    pub fn lock_at(&self, escrow: &Address, id: &[u8; 32], tag: &str) -> Res<Lock> {
        Lock::decode(&self.call_at(escrow, &calldata("locks(bytes32)", &[Word::Bytes32(*id)]), tag)?)
    }

    pub fn vault_implementation(&self, escrow: &Address) -> Res<Address> {
        word_address(&self.call_at(escrow, &calldata("vaultImplementation()", &[]), "latest")?, 0)
    }

    pub fn is_blocked(&self, token: &Address, who: &Address, tag: &str) -> Res<bool> {
        word_bool(&self.call_at(token, &calldata("isBlocked(address)", &[Word::Address(*who)]), tag)?, 0)
    }

    pub fn token_balance(&self, token: &Address, who: &Address, tag: &str) -> Res<u128> {
        word_u128(&self.call_at(token, &calldata("balanceOf(address)", &[Word::Address(*who)]), tag)?, 0)
    }

    pub fn allowance(&self, token: &Address, owner: &Address, spender: &Address) -> Res<u128> {
        let data = calldata("allowance(address,address)", &[Word::Address(*owner), Word::Address(*spender)]);
        word_u128(&self.call_at(token, &data, "latest")?, 0)
    }

    pub fn balance(&self, who: &Address, tag: &str) -> Res<u128> {
        quantity(&self.call("eth_getBalance", json!([who.to_string(), tag]))?)
    }

    /// `pending` counts transactions in the node's mempool; `latest` only mined ones.
    pub fn nonce(&self, who: &Address, tag: &str) -> Res<u64> {
        quantity_u64(&self.call("eth_getTransactionCount", json!([who.to_string(), tag]))?)
    }

    pub fn code(&self, who: &Address) -> Res<Vec<u8>> {
        from_hex(self.call("eth_getCode", json!([who.to_string(), "latest"]))?.as_str().unwrap_or_default())
    }

    /// The latest block's base fee, and a priority fee the node suggests.
    pub fn fees(&self) -> Res<(u128, u128)> {
        let latest = self.call("eth_getBlockByNumber", json!(["latest", false]))?;
        let base = quantity(&latest["baseFeePerGas"])?;
        let priority = quantity(&self.call("eth_maxPriorityFeePerGas", json!([]))?)?;
        Ok((base, priority))
    }

    pub fn estimate_gas(&self, from: &Address, to: &Address, data: &[u8], value: u128) -> Res<u64> {
        let tx = json!({ "from": from.to_string(), "to": to.to_string(), "data": format!("0x{}", to_hex(data)),
                         "value": format!("0x{value:x}") });
        quantity_u64(&self.call("eth_estimateGas", json!([tx]))?)
    }

    /// Sends a signed transaction, returning its hash. A node that already has it may answer "already known", which counts as
    /// sent: the hash is the transaction's own.
    pub fn send_raw(&self, raw: &[u8]) -> Res<[u8; 32]> {
        match self.call("eth_sendRawTransaction", json!([format!("0x{}", to_hex(raw))])) {
            Ok(_) => Ok(crate::evm::tx_hash(raw)),
            Err(e) if e.contains("already known") || e.contains("known transaction") => Ok(crate::evm::tx_hash(raw)),
            Err(e) => Err(e),
        }
    }

    pub fn receipt(&self, hash: &[u8; 32]) -> Res<Option<Receipt>> {
        let r = self.call("eth_getTransactionReceipt", json!([format!("0x{}", to_hex(hash))]))?;
        if r.is_null() {
            return Ok(None);
        }
        Ok(Some(Receipt {
            success: quantity(&r["status"])? == 1,
            block: quantity_u64(&r["blockNumber"])?,
            gas_used: quantity_u64(&r["gasUsed"])?,
        }))
    }
}
