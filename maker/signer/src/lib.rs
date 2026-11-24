//! The maker's signer. From liq2sol's (docs/copied.md), with Polygon in Solana's place: docs/design.md section 4.2.
//!
//! This process holds every hot key, and it is the only thing that can spend. It treats whoever is on the socket as
//! possibly compromised, so:
//!
//! - It builds each transaction itself. Our claims and refunds pay our own wallet, because nothing else can be asked for.
//! - It reads heights, blocks, coins and locks through its own chain connections, and never takes them from a request.
//! - It runs `policy::check`, commits the result durably, and only then releases a signature.
//! - On Polygon, a nonce carries one call forever: the same call may be signed again at a higher fee, never another.
//! - It never broadcasts, and it never returns or logs a key.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use liq2any_chain::esplora::{Esplora, Rpc};
use liq2any_chain::evm::{self, address_of, calldata, keccak256, Address as EvmAddress, Tx, Word};
use liq2any_chain::liquid::{
    self, keypair_from, sign_keypath, usdt, use_network, NetworkConfig, Payment, SecretCoin, Wallet, FEE,
};
use liq2any_chain::polygon::{
    self, approve_call, claim_call, extend_call, lock_call, lock_id, refund_call, release_call, vault_lock_call,
    Rpc as PolygonRpc, State as LockState,
};
use liq2any_chain::swap::{check_claim_shape, spent_outputs, swap_address, swap_output, taker_fee_address};
use liq2any_maker_core::engine::Direction;
use liq2any_maker_core::policy::{
    self, secs_left, CoinEvidence, EscrowEvidence, Ledger, Limits, PolygonClock, Refusal, Request,
};
use liq2any_maker_core::store::{SignedCall, SignerLedger};
use liq2any_maker_core::{CompressedKey, LockPoint};
use liq2any_swapkey::{
    adaptor_bytes, adaptor_from, combine, complete, extract_secret, key_spend_sighash, nonce_bytes, nonce_from,
    polygon_address_of, polygon_value, set_key_spend_witness, share_bytes, share_from, verify_share, Nonce, Session,
    SwapOutput,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use simplicityhl::elements::encode::{deserialize, serialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::{Keypair as LiquidKey, Message, Secp256k1, SecretKey};
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, Sequence, Transaction as LiquidTx};

/// Longest accepted request line. The largest real request carries a blinded claim, about 12 KB as hex.
pub const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// Fee coins are topped up below this many free ones, by this many at a time.
const FEE_COINS_LOW: usize = 4;
const FEE_COINS_TOP_UP: usize = 8;
/// Open connections beyond this are closed at once, so a flood can't exhaust threads.
pub const MAX_CONNECTIONS: usize = 16;

/// Gas limits, fixed per call so nothing depends on an estimate. Each is well above what the call used on a fork of Polygon
/// mainnet and on Amoy (contracts/polygon/README.md); gas that isn't used isn't paid for.
pub const LOCK_GAS: u64 = 400_000;
/// A lock through a maker vault: its approval and cap on top of the escrow's lock, about 280,000 measured.
pub const VAULT_LOCK_GAS: u64 = 450_000;
pub const CLAIM_GAS: u64 = 250_000;
pub const REFUND_GAS: u64 = 200_000;
pub const RELEASE_GAS: u64 = 200_000;
pub const EXTEND_GAS: u64 = 120_000;
pub const APPROVE_GAS: u64 = 120_000;
pub const TOKEN_TRANSFER_GAS: u64 = 150_000;
pub const POL_TRANSFER_GAS: u64 = 21_000;
/// How many blocks back the block time is measured over.
const BLOCK_TIME_SPAN: u64 = 2_000;
/// POL kept for each P→L swap locked on Liquid and not yet claimed: a claim's gas at a high fee, with room to spare.
const POL_KEPT_PER_P2L_LOCK: u128 = 500_000_000_000_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LiquidSource {
    Esplora(String),
    Node { cli: String, args: Vec<String> },
}

/// Polygon settings. On a test network the escrow and token come from here; on mainnet they are pinned in the code, and
/// setting them here refuses to start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolygonConfig {
    pub rpc: String,
    #[serde(default)]
    pub escrow: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub fees: PolygonFees,
    /// A `MakerVault` (docs/design.md section 2.6) whose operator is our Polygon key. With one, the vault is our Polygon
    /// address in every swap and holds the USDT0; our key only signs and pays gas. Without one, our key does both.
    #[serde(default)]
    pub maker_vault: Option<String>,
}

/// EIP-1559 fees, in gwei. Polygon's validators require a priority fee of at least 25 gwei on mainnet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolygonFees {
    pub priority_floor_gwei: u64,
    pub max_fee_cap_gwei: u64,
}

impl Default for PolygonFees {
    fn default() -> Self {
        Self { priority_floor_gwei: 30, max_fee_cap_gwei: 3_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// "testnet" (the default) or "mainnet". Both chains must agree with it, or the signer won't start.
    #[serde(default = "default_network")]
    pub network: String,
    pub socket: PathBuf,
    pub ledger: PathBuf,
    pub keys_dir: PathBuf,
    pub liquid: LiquidSource,
    pub polygon: PolygonConfig,
    pub limits: Limits,
    /// Regtest assets as (stand-in USDt, L-BTC). Liquid testnet is the default.
    pub regtest_assets: Option<(String, String)>,
    /// What each fee coin holds. Only the relay floor limits it: a claim needs about 26 sats, and a refund 29, at 0.1 sat/vB.
    #[serde(default = "default_fee_coin")]
    pub fee_coin_sats: u64,
    /// Pay each claim, refund, change and fee-coin top-up to a new address of our own, so our coins can't be linked by
    /// address. Off by default. It needs `liquid` to be our own node: an API would see one IP asking about every address.
    #[serde(default)]
    pub fresh_addresses: bool,
    /// Where the operator's withdrawals go: a confidential Liquid address and a Polygon address, both the operator's own and
    /// ideally cold. Only here, never in a request, so nothing reaching the socket can choose where funds go. Without one, that
    /// chain's withdrawals are refused.
    #[serde(default)]
    pub withdraw_to: WithdrawTo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WithdrawTo {
    pub liquid: Option<String>,
    pub polygon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chain {
    Liquid,
    Polygon,
}

/// The stablecoin, or what pays fees: L-BTC on Liquid, POL on Polygon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Asset {
    Usdt,
    Gas,
}

pub fn default_network() -> String {
    "testnet".into()
}

fn default_fee_coin() -> u64 {
    FEE
}

/// One swap's agreed terms. The same for both directions, apart from who refunds. Nothing here is trusted: a wrong value
/// only leads to an address holding no coin, a lock that isn't there, or a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapTerms {
    pub direction: Direction,
    pub lock_point: LockPoint,
    pub taker_key: CompressedKey,
    /// Generated by the taker and shared in encrypted RFQ, so both can read the coin.
    pub blinding_key: [u8; 32],
    pub lock_height: u32,
    pub liquid_amount: u64,
    /// USDT0 base units.
    pub polygon_amount: u64,
    /// The taker's Polygon address: the recipient of our L→P lock, or the initiator of its P→L lock.
    pub taker_polygon: String,
    /// L→P: where our lock forwards the gas drop, and how much, in wei.
    #[serde(default)]
    pub gas_to: Option<String>,
    #[serde(default)]
    pub gas_drop: u128,
}

impl SwapTerms {
    pub fn swap_id(&self) -> [u8; 32] {
        self.lock_point.id()
    }

    pub fn blinding(&self) -> Result<SecretKey, String> {
        SecretKey::from_slice(&self.blinding_key).map_err(|e| format!("bad blinding key: {e}"))
    }

    /// Who may refund: the party that funded the coin.
    pub fn output(&self, maker_key: &CompressedKey) -> Result<SwapOutput, String> {
        swap_output(&maker_key.0, &self.taker_key.0, self.direction == Direction::L2P, self.lock_height)
    }

    /// The confidential address to fund. Both parties derive it, and neither takes it from the other.
    pub fn address(&self, maker_key: &CompressedKey) -> Result<Address, String> {
        Ok(swap_address(&self.output(maker_key)?, &self.blinding()?))
    }

    /// P→L: where the taker's fee coin goes.
    pub fn taker_fee_address(&self) -> Result<Address, String> {
        taker_fee_address(&self.taker_key.0, &self.blinding()?)
    }

    pub fn taker(&self) -> Result<EvmAddress, String> {
        EvmAddress::parse(&self.taker_polygon).map_err(|e| format!("bad taker address: {e}"))
    }
}

/// The Polygon form of a lock point: the address the escrow stores.
fn lock_address(lock_point: &LockPoint) -> Result<EvmAddress, String> {
    Ok(EvmAddress(polygon_address_of(&lock_point.0).map_err(|e| e.to_string())?))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Call {
    Identity,
    /// L→P: never lock this swap, so its refund can be signed later. Nothing is signed now.
    Cancel { terms: SwapTerms },
    /// L→P: sign the taker's early refund, and never lock this swap.
    EarlyRefund { terms: SwapTerms, tx: String, taker_nonce: String },
    SwapKey { swap_id: [u8; 32] },
    /// Whether a checked adapter signature is stored for this swap.
    AdaptorHeld { swap_id: [u8; 32] },
    /// Keep a pool of fee coins ready. Only ever pays our own wallet.
    TopUpFeeCoins,
    /// Let the escrow take our USDT0 for our own locks. The escrow only ever pulls from whoever calls `lock`.
    ApproveEscrow,
    /// What we hold and could lock, for the offer. Nothing is signed.
    Capacity,
    /// Everything the wallets hold, for the operator's report. Nothing is signed.
    Balances,
    /// L→P, step one: check the taker's coin, build our claim, and start a signing session on it.
    PrepareClaim { terms: SwapTerms },
    /// L→P, step two: check the taker's share, and keep the adapter signature for the claim.
    StoreAdaptor { terms: SwapTerms, taker_nonce: String, taker_share: String },
    LockPolygon { terms: SwapTerms, deadline_time: u64, deadline_block: u64 },
    LockLiquid { terms: SwapTerms },
    /// P→L, step one: the taker's claim and nonce. Replies with our nonce.
    OpenSession { terms: SwapTerms, claim_tx: String, taker_nonce: String },
    /// P→L, step two: the taker's share. We keep the adapter signature first, and only then reply with our share.
    FinishSession { terms: SwapTerms, taker_share: String },
    /// P→L: the secret, from the signature the taker published.
    ExtractSecret { lock_point: LockPoint, signature: String },
    ClaimLiquid { terms: SwapTerms, secret: [u8; 32] },
    ClaimPolygon { taker_polygon: String, lock_point: LockPoint, secret: [u8; 32] },
    RefundLiquid { terms: SwapTerms },
    RefundPolygon { lock_point: LockPoint },
    /// The operator's withdrawal, in the chain's base units, or all of it. It always goes to `withdraw_to` in the config: this
    /// carries no destination, so a compromised caller can at worst move funds to the operator's own address. 128 bits, since
    /// 64 bits of wei is only 18.4 POL.
    Withdraw { chain: Chain, asset: Asset, amount: Option<u128> },
    Extend { taker_polygon: String, lock_point: LockPoint, new_time: u64, new_block: u64 },
    Release { taker_polygon: String, lock_point: LockPoint },
    /// Every Polygon transaction signed and not yet mined, lowest nonce first, re-signed at a higher fee where the network's has
    /// risen past it. A stuck nonce would hold up everything after it, a claim included, so the daemon sends these each tick.
    PendingPolygon,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Identity { liquid_address: String, polygon: String, fee_coin_sats: u64 },
    SwapKey { key: CompressedKey },
    Held(bool),
    /// In base units of each chain's token.
    Capacity { polygon_units: u64, liquid_units: u64 },
    /// Our wallets, for the report. Liquid in its base units, fee coins counted apart from the rest of the L-BTC.
    Balances {
        liquid_usdt: u64,
        liquid_lbtc_sats: u64,
        free_fee_coins: u64,
        polygon_usdt: u64,
        pol_wei: u128,
        polygon_allowance: u128,
    },
    /// Nothing needed doing.
    Nothing,
    /// L→P: our unsigned claim and nonce, for the taker to sign.
    ClaimTemplate { tx: String, nonce: String },
    /// P→L: our nonce, for the taker's claim.
    Nonce(String),
    EarlyRefundShare { nonce: String, share: String },
    /// P→L: our share. The same bytes on every repeat.
    Share(String),
    Secret([u8; 32]),
    /// A signed Liquid transaction, in hex. The caller broadcasts it.
    Liquid(String),
    /// An earlier Liquid withdrawal, signed and not yet seen confirmed: the same bytes, to send again. Nothing new was signed.
    EarlierWithdrawal(String),
    /// A signed Polygon transaction, in hex, with its nonce and hash. The caller broadcasts it.
    Polygon { raw: String, nonce: u64, hash: String },
    /// Signed Polygon transactions not yet mined, lowest nonce first.
    PolygonPending(Vec<String>),
    Refused(Refusal),
    /// Could not decide, such as a chain read failing. Nothing was signed or recorded.
    Failed(String),
}

/// What a finished session leaves. Losing this after acting on it loses the swap, so it goes in the ledger first.
#[derive(Serialize, Deserialize)]
struct HeldAdaptor {
    adaptor: String,
    /// L→P: our claim, without its witnesses.
    tx: String,
    /// P→L: our share, resent as often as asked, because repeating it reveals nothing new.
    share: Option<String>,
}

/// A session between its two steps. Memory only: the nonce must never reach a disk, and a crash abandons the session.
struct OpenSession {
    terms: SwapTerms,
    tx: LiquidTx,
    message: [u8; 32],
    session: Session,
    their_nonce: Option<Nonce>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())).collect()
}

/// Our wallet at an index. 0 is the key files themselves. Each other index has its own spend and blinding keys, derived
/// from both, so a restored signer with its ledger finds every coin.
fn wallet_at(secrets: &([u8; 32], [u8; 32]), idx: u32) -> Result<Wallet, String> {
    let (spend, blinding) = if idx == 0 {
        (secrets.0, secrets.1)
    } else {
        let derive = |tag: &[u8], secret: &[u8; 32]| -> [u8; 32] {
            Sha256::new().chain_update(tag).chain_update(secret).chain_update(idx.to_le_bytes()).finalize().into()
        };
        (derive(b"liq2any-wallet-spend-v1", &secrets.0), derive(b"liq2any-wallet-blinding-v1", &secrets.1))
    };
    Ok(Wallet { key: keypair_from(&spend)?, blinding: SecretKey::from_slice(&blinding).map_err(|e| e.to_string())? })
}

fn load_or_create_secret(path: &Path) -> Result<[u8; 32], String> {
    if !path.exists() {
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(|e| e.to_string())?;
        std::fs::write(path, secret).map_err(|e| e.to_string())?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::read(path).map_err(|e| e.to_string())?.try_into().map_err(|_| "bad key file".to_string())
}

fn outpoint_text(c: &SecretCoin) -> String {
    format!("{}:{}", c.coin.outpoint.txid, c.coin.outpoint.vout)
}

fn spends_of(tx: &LiquidTx) -> Vec<String> {
    tx.input.iter().map(|i| format!("{}:{}", i.previous_output.txid, i.previous_output.vout)).collect()
}

/// What identifies a Polygon call for the nonce rule: its destination, value and data.
pub fn call_hash(to: &EvmAddress, value: u128, data: &[u8]) -> [u8; 32] {
    let mut bytes = to.0.to_vec();
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes.extend_from_slice(data);
    keccak256(&bytes)
}

/// A replacement must outbid the transaction it replaces by more than nodes' 10%: 12.5%, plus one wei.
pub fn outbid(previous: u128) -> u128 {
    previous.saturating_add(previous / 8).saturating_add(1)
}

pub struct Signer {
    limits: Limits,
    chain: Esplora,
    polygon: PolygonRpc,
    ids: polygon::Ids,
    fees: PolygonFees,
    ledger: SignerLedger,
    genesis: BlockHash,
    /// Index 0, the address we are funded at. With fresh addresses, the others derive from its keys.
    wallet: Wallet,
    wallet_secrets: ([u8; 32], [u8; 32]),
    fresh_addresses: bool,
    polygon_key: evm::SecretKeyEvm,
    /// Our key's address: it signs, pays gas, and keeps the nonce.
    me: EvmAddress,
    /// The maker vault, if any. `holder()` is the address swaps see.
    vault: Option<EvmAddress>,
    swap_seed: [u8; 32],
    fee_coin_sats: u64,
    sessions: HashMap<[u8; 32], OpenSession>,
    withdraw_liquid: Option<Address>,
    withdraw_polygon: Option<EvmAddress>,
}

/// Refuse, or fail, without signing anything.
enum Stop {
    Refused(Refusal),
    Failed(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

impl From<&str> for Stop {
    fn from(e: &str) -> Self {
        Stop::Failed(e.to_string())
    }
}

impl From<Refusal> for Stop {
    fn from(r: Refusal) -> Self {
        Stop::Refused(r)
    }
}

type Outcome = Result<Reply, Stop>;

impl Signer {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        if cfg.fresh_addresses && !matches!(cfg.liquid, LiquidSource::Node { .. }) {
            return Err(
                "refusing to start: fresh_addresses needs liquid to be our own node, because an API would see one IP \
                        asking about every address"
                    .into(),
            );
        }
        let chain = match &cfg.liquid {
            LiquidSource::Esplora(url) => Esplora::Http(url.clone()),
            LiquidSource::Node { cli, args } => Esplora::Node(Rpc { cli: cli.clone(), args: args.clone() }),
        };
        let polygon = PolygonRpc::new(&cfg.polygon.rpc);
        let genesis = chain.genesis()?;
        let parse = |what: &str, s: &Option<String>| -> Result<Option<EvmAddress>, String> {
            s.as_deref().map(|a| EvmAddress::parse(a).map_err(|e| format!("refusing to start: polygon.{what}: {e}"))).transpose()
        };
        let test_ids = match (parse("escrow", &cfg.polygon.escrow)?, parse("token", &cfg.polygon.token)?) {
            (Some(escrow), Some(token)) => Some((escrow, token)),
            (None, None) => None,
            _ => return Err("refusing to start: set both polygon.escrow and polygon.token, or neither".into()),
        };
        // Before any key is loaded.
        polygon::enter(
            &cfg.network,
            cfg.regtest_assets.is_some(),
            &[cfg.limits.max_per_swap],
            &genesis,
            polygon.chain_id()?,
            test_ids,
        )?;
        let ids = polygon::ids();
        if polygon.code(&ids.escrow)?.is_empty() {
            return Err("refusing to start: there is no escrow contract at the configured address".into());
        }
        if let Some((usdt_id, lbtc_id)) = &cfg.regtest_assets {
            use_network(NetworkConfig {
                params: &AddressParams::ELEMENTS,
                usdt: AssetId::from_str(usdt_id).map_err(|e| e.to_string())?,
                lbtc: AssetId::from_str(lbtc_id).map_err(|e| e.to_string())?,
            });
        }
        // A bad address is a mistake in the config, not an outage, so it refuses: anything else is retried for ever.
        let withdraw_liquid = cfg.withdraw_to.liquid.as_deref().map(withdrawal_address).transpose()?;
        let withdraw_polygon = cfg
            .withdraw_to
            .polygon
            .as_deref()
            .map(|s| EvmAddress::parse(s).map_err(|e| format!("refusing to start: withdraw_to.polygon: {e}")))
            .transpose()?;
        std::fs::create_dir_all(&cfg.keys_dir).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&cfg.keys_dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        let wallet_secrets = (
            load_or_create_secret(&cfg.keys_dir.join("liquid-wallet.key"))?,
            load_or_create_secret(&cfg.keys_dir.join("liquid-blinding.key"))?,
        );
        let wallet = wallet_at(&wallet_secrets, 0)?;
        let swap_seed = load_or_create_secret(&cfg.keys_dir.join("swap-seed.key"))?;
        let polygon_key = evm::SecretKeyEvm::from_slice(&load_or_create_secret(&cfg.keys_dir.join("polygon.key"))?)
            .map_err(|e| format!("bad Polygon key: {e}"))?;
        let me = address_of(&polygon_key);
        let vault = parse("maker_vault", &cfg.polygon.maker_vault)?;
        if let Some(vault) = &vault {
            check_vault(
                &polygon.vault_state(vault).map_err(|e| format!("refusing to start: polygon.maker_vault: {e}"))?,
                &ids,
                &me,
            )?;
        }
        Ok(Self {
            limits: cfg.limits.clone(),
            chain,
            polygon,
            ids,
            fees: cfg.polygon.fees,
            ledger: SignerLedger::open(&cfg.ledger).map_err(|e| e.to_string())?,
            genesis: BlockHash::from_str(&genesis).map_err(|e| e.to_string())?,
            wallet,
            wallet_secrets,
            fresh_addresses: cfg.fresh_addresses,
            polygon_key,
            me,
            vault,
            swap_seed,
            fee_coin_sats: cfg.fee_coin_sats,
            sessions: HashMap::new(),
            withdraw_liquid,
            withdraw_polygon,
        })
    }

    /// One key per swap, derived so that a restored seed can still spend every open swap.
    fn swap_key(&self, swap_id: &[u8; 32]) -> Result<LiquidKey, String> {
        let digest: [u8; 32] = Sha256::new()
            .chain_update(b"liq2any-swap-key-v1")
            .chain_update(self.swap_seed)
            .chain_update(swap_id)
            .finalize()
            .into();
        keypair_from(&digest)
    }

    fn maker_key(&self, swap_id: &[u8; 32]) -> Result<CompressedKey, String> {
        Ok(LockPoint(self.swap_key(swap_id)?.public_key().serialize()))
    }

    /// The swap's coin, read with the swap's blinding key. A coin that isn't exactly this asset and amount doesn't count.
    fn swap_coin(&self, t: &SwapTerms) -> Result<Option<SecretCoin>, String> {
        let address = t.address(&self.maker_key(&t.swap_id())?)?;
        let coins = self.chain.secret_coins(&address, &t.blinding()?)?;
        Ok(liquid::exact_secret_coin(&coins, usdt(), t.liquid_amount, self.chain.tip()?, 0))
    }

    /// Our own blinded coins. An explicit coin is ignored, whoever sent it: spending one would bound our blinded outputs.
    fn wallet_coins(&self) -> Result<Vec<SecretCoin>, String> {
        let at: Vec<_> = self.wallets()?.iter().map(|w| (w.address(), w.blinding)).collect();
        let coins = self.chain.secret_coins_at(&at)?;
        Ok(coins.into_iter().filter(|c| !c.coin.out.asset.is_explicit() && !c.coin.out.value.is_explicit()).collect())
    }

    /// Every address we hold coins at: just the first, unless fresh addresses are on.
    fn wallets(&self) -> Result<Vec<Wallet>, String> {
        let mut all = vec![self.wallet.clone()];
        if self.fresh_addresses {
            for idx in self.ledger.address_indexes().map_err(|e| e.to_string())? {
                all.push(wallet_at(&self.wallet_secrets, idx)?);
            }
        }
        Ok(all)
    }

    /// The key for one of our coins, found by its script.
    fn key_for(
        wallets: &[Wallet],
        script: &simplicityhl::elements::Script,
    ) -> Option<simplicityhl::elements::schnorr::Keypair> {
        wallets.iter().find(|w| w.address().script_pubkey() == *script).map(|w| w.key)
    }

    /// Where to pay ourselves. A swap keeps one address, so a retry pays the same one. Recorded before it is used.
    fn receive_address(&self, swap: Option<&[u8; 32]>) -> Result<Address, String> {
        if !self.fresh_addresses {
            return Ok(self.wallet.address());
        }
        let idx = self.ledger.address_index(swap).map_err(|e| e.to_string())?;
        Ok(wallet_at(&self.wallet_secrets, idx)?.address())
    }

    /// Pay from our own coins, with change to `change`.
    fn pay(&self, payments: Vec<Payment>, change: &Address) -> Result<LiquidTx, String> {
        let wallets = self.wallets()?;
        liquid::pay_from_coins_with(&self.payable_coins()?, change, |s| Self::key_for(&wallets, s), payments, self.genesis)
    }

    /// Sign the fee coin, input 1 of a swap spend, with the key of the address it is at.
    fn sign_fee_coin(&self, tx: &mut LiquidTx, utxos: &[simplicityhl::elements::TxOut]) -> Result<(), String> {
        let script = &utxos.get(1).ok_or("no fee coin input")?.script_pubkey;
        let key = Self::key_for(&self.wallets()?, script).ok_or("the fee coin isn't at one of our addresses")?;
        sign_keypath(tx, utxos, 1, &key, self.genesis)
    }

    fn is_fee_coin(&self, c: &SecretCoin) -> bool {
        c.secrets.asset == liquid::lbtc() && c.secrets.value == self.fee_coin_sats
    }

    fn free_fee_coins(&self, coins: Vec<SecretCoin>) -> Result<Vec<SecretCoin>, String> {
        let mut free = vec![];
        for c in coins {
            if self.is_fee_coin(&c) && !self.ledger.is_reserved(&outpoint_text(&c)).map_err(|e| e.to_string())? {
                free.push(c);
            }
        }
        Ok(free)
    }

    /// The coins ordinary payments may use. Fee coins stay out, or a payment could kill a signature that commits to one.
    fn payable_coins(&self) -> Result<Vec<SecretCoin>, String> {
        let mut coins = vec![];
        for c in self.wallet_coins()? {
            if !self.is_fee_coin(&c) && !self.ledger.is_used(&outpoint_text(&c)).map_err(|e| e.to_string())? {
                coins.push(c);
            }
        }
        Ok(coins)
    }

    /// This swap's fee coin, reserved durably before anything commits to it. It must be confirmed: a signature on a coin
    /// whose parent later drops could never be used.
    fn fee_coin_for(&self, swap_id: &[u8; 32]) -> Result<SecretCoin, String> {
        let coins = self.wallet_coins()?;
        if let Some(text) = self.ledger.fee_coin(swap_id).map_err(|e| e.to_string())? {
            return coins.into_iter().find(|c| outpoint_text(c) == text).ok_or_else(|| "this swap's fee coin is gone".to_string());
        }
        let free = self.free_fee_coins(coins)?;
        let coin = free.into_iter().find(|c| c.coin.height.is_some()).ok_or("no confirmed fee coin is ready")?;
        if !self.ledger.reserve_fee_coin(swap_id, &outpoint_text(&coin)).map_err(|e| e.to_string())? {
            return Err("the fee coin was taken".into());
        }
        Ok(coin)
    }

    /// Only ever pays our own wallet. One at a time, and a retry sends the same bytes, so a top-up that is on its way can't be
    /// paid again, and can't take coins a lock needs.
    fn top_up_fee_coins(&self) -> Outcome {
        if let Some(raw) = self.ledger.pending_top_up().map_err(|e| e.to_string())? {
            let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
            if !self.chain.is_confirmed(&tx.txid()).unwrap_or(false) {
                return Ok(Reply::Liquid(hex(&raw)));
            }
            self.ledger.clear_pending_top_up().map_err(|e| e.to_string())?;
        }
        if self.free_fee_coins(self.wallet_coins()?)?.len() >= FEE_COINS_LOW {
            return Ok(Reply::Nothing);
        }
        let to = self.receive_address(None)?;
        let payments =
            (0..FEE_COINS_TOP_UP).map(|_| Payment { asset: liquid::lbtc(), amount: self.fee_coin_sats, address: to.clone() }).collect();
        let tx = self.pay(payments, &to)?;
        let raw = serialize(&tx);
        self.ledger.set_pending_top_up(&raw, &spends_of(&tx)).map_err(|e| e.to_string())?;
        Ok(Reply::Liquid(hex(&raw)))
    }

    /// Our Polygon address in swaps: the vault's when there is one, else our key's.
    fn holder(&self) -> EvmAddress {
        self.vault.unwrap_or(self.me)
    }

    fn polygon_usdt(&self) -> Result<u64, String> {
        let balance = self.polygon.token_balance(&self.ids.token, &self.holder(), "latest")?;
        u64::try_from(balance).map_err(|_| "a USDT0 balance over 64 bits".to_string())
    }

    /// What we can lock now: the USDT0 held, and with a vault, no more than its window still allows.
    fn capacity(&self) -> Outcome {
        let liquid_units = self.payable_coins()?.iter().filter(|c| c.secrets.asset == usdt()).map(|c| c.secrets.value).sum();
        let mut polygon_units = self.polygon_usdt()?;
        if let Some(vault) = &self.vault {
            let room = self.polygon.vault_state(vault)?.room(self.polygon.block("latest")?.time);
            polygon_units = polygon_units.min(u64::try_from(room).unwrap_or(u64::MAX));
        }
        Ok(Reply::Capacity { polygon_units, liquid_units })
    }

    fn balances(&self) -> Outcome {
        let coins = self.wallet_coins()?;
        let sum = |asset| coins.iter().filter(|c| c.secrets.asset == asset).map(|c| c.secrets.value).sum::<u64>();
        let (liquid_usdt, liquid_lbtc_sats) = (sum(usdt()), sum(liquid::lbtc()));
        let free_fee_coins = self.free_fee_coins(coins)?.len() as u64;
        Ok(Reply::Balances {
            liquid_usdt,
            liquid_lbtc_sats,
            free_fee_coins,
            polygon_usdt: self.polygon_usdt()?,
            pol_wei: self.polygon.balance(&self.me, "latest")?,
            // A vault approves each lock itself, so there is nothing for us to approve.
            polygon_allowance: match self.vault {
                Some(_) => u128::MAX,
                None => self.polygon.allowance(&self.ids.token, &self.me, &self.ids.escrow)?,
            },
        })
    }

    /// To the operator's own address, from `withdraw_to`. One at a time, and a retry gets the same bytes, like a top-up. It
    /// spends only coins nothing else has a claim on: fee coins promised to a swap stay, and so do coins a payment has used.
    fn withdraw_liquid(&self, asset: Asset, amount: Option<u128>) -> Outcome {
        let amount = amount.map(u64::try_from).transpose().map_err(|_| "a Liquid amount is at most 64 bits")?;
        let to = self.withdraw_liquid.clone().ok_or("no Liquid withdrawal address is set in withdraw_to")?;
        if let Some(raw) = self.ledger.pending_withdrawal().map_err(|e| e.to_string())? {
            let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
            if !self.chain.is_confirmed(&tx.txid()).unwrap_or(false) {
                return Ok(Reply::EarlierWithdrawal(hex(&raw)));
            }
            self.ledger.clear_pending_withdrawal().map_err(|e| e.to_string())?;
        }
        let wallets = self.wallets()?;
        let key_for = |s: &simplicityhl::elements::Script| Self::key_for(&wallets, s);
        let mut coins = self.payable_coins()?;
        let tx = match (asset, amount) {
            (_, Some(0)) => return Err("nothing to withdraw".to_string().into()),
            (Asset::Usdt, amount) => {
                let held: u64 = coins.iter().filter(|c| c.secrets.asset == usdt()).map(|c| c.secrets.value).sum();
                let amount = amount.unwrap_or(held);
                if amount == 0 {
                    return Err("there is no USDt to withdraw".to_string().into());
                }
                liquid::pay_from_coins_with(
                    &coins,
                    &self.receive_address(None)?,
                    key_for,
                    vec![Payment { asset: usdt(), amount, address: to }],
                    self.genesis,
                )?
            }
            (Asset::Gas, Some(amount)) => liquid::pay_from_coins_with(
                &coins,
                &self.receive_address(None)?,
                key_for,
                vec![Payment { asset: liquid::lbtc(), amount, address: to }],
                self.genesis,
            )?,
            // All of it, the fee coins nothing is promised to included.
            (Asset::Gas, None) => {
                coins.extend(self.free_fee_coins(self.wallet_coins()?)?);
                liquid::sweep_lbtc_with(&coins, &to, key_for, self.genesis)?
            }
        };
        let raw = serialize(&tx);
        self.ledger.set_pending_withdrawal(&raw, &spends_of(&tx)).map_err(|e| e.to_string())?;
        Ok(Reply::Liquid(hex(&raw)))
    }

    /// To the operator's own address, from `withdraw_to`. A P→L swap we have locked on Liquid is only ours to finish by claiming
    /// the taker's lock, which costs POL. Without it the deadline passes, the taker refunds, and keeps our Liquid coin too. So POL
    /// for each stays, whatever is asked.
    fn withdraw_polygon(&self, asset: Asset, amount: Option<u128>) -> Outcome {
        let to = self.withdraw_polygon.ok_or("no Polygon withdrawal address is set in withdraw_to")?;
        let none = [0u8; 32];
        match (asset, amount) {
            (_, Some(0)) => Err("nothing to withdraw".to_string().into()),
            (Asset::Usdt, _) if self.vault.is_some() => {
                Err("the vault's USDT0 is withdrawn by its owner, not by this key".to_string().into())
            }
            (Asset::Usdt, amount) => {
                let amount = match amount {
                    Some(a) => a,
                    None => u128::from(self.polygon_usdt()?),
                };
                if amount == 0 {
                    return Err("there is no USDT0 to withdraw".to_string().into());
                }
                let data = calldata("transfer(address,uint256)", &[Word::Address(to), Word::Uint(amount)]);
                let signed = self.sign_polygon(self.ids.token, 0, data, TOKEN_TRANSFER_GAS, true)?;
                self.commit_polygon(&none, &signed)?;
                Ok(polygon_reply(&signed))
            }
            (Asset::Gas, amount) => {
                let unsettled = self.ledger.unsettled_p2l_locks().map_err(|e| e.to_string())?;
                let held = self.polygon.balance(&self.me, "latest")?;
                let (max_fee, _) = self.polygon_fees()?;
                let gas_cost = u128::from(POL_TRANSFER_GAS).saturating_mul(max_fee);
                let amount = match amount {
                    Some(a) => a,
                    // All but the gas for this transfer and the claims still to make.
                    None => held.saturating_sub(gas_cost).saturating_sub(pol_kept(unsettled)),
                };
                pol_left_is_enough(unsettled, held.saturating_sub(amount).saturating_sub(gas_cost))?;
                if amount == 0 {
                    return Err("there is no POL to withdraw".to_string().into());
                }
                let signed = self.sign_polygon(to, amount, vec![], POL_TRANSFER_GAS, false)?;
                self.commit_polygon(&none, &signed)?;
                Ok(polygon_reply(&signed))
            }
        }
    }

    /// Check policy, persist, and only then hand back the signed bytes. A crash before the commit releases nothing.
    fn check(&self, request: &Request) -> Result<policy::SwapRecord, Stop> {
        Ok(policy::check(&self.limits, &self.ledger, now_secs(), request)?)
    }

    fn commit(
        &self,
        swap_id: &[u8; 32],
        request: &Request,
        record: &policy::SwapRecord,
        lock_tx: Option<(&[u8], &[String])>,
        polygon: Option<&SignedCall>,
    ) -> Result<(), Stop> {
        let before = self.ledger.record(swap_id);
        let newly = policy::newly_locked(&before, request).map(|amount| (now_secs(), amount));
        let (raw, spends) = lock_tx.map_or((None, &[][..]), |(raw, spends)| (Some(raw), spends));
        self.ledger.commit(swap_id, record, newly, raw, spends, polygon).map_err(|e| Stop::Failed(e.to_string()))
    }

    /// Record a Polygon transaction that touches no swap, such as an approval or a withdrawal.
    fn commit_polygon(&self, swap_id: &[u8; 32], signed: &SignedCall) -> Result<(), Stop> {
        let record = self.ledger.record(swap_id);
        self.ledger.commit(swap_id, &record, None, None, &[], Some(signed)).map_err(|e| Stop::Failed(e.to_string()))
    }

    // ---- Polygon ----

    /// The finalized block, and the block time measured over the blocks before it, all from our own connection.
    fn polygon_clock(&self) -> Result<PolygonClock, String> {
        let f = self.polygon.block("finalized")?;
        let past = self.polygon.block_number_at(f.number.saturating_sub(BLOCK_TIME_SPAN))?;
        let blocks = f.number.saturating_sub(past.number).max(1);
        let block_ms = f.time.saturating_sub(past.time).saturating_mul(1_000) / blocks;
        Ok(PolygonClock { time: f.time, block: f.number, block_ms })
    }

    /// The fees to sign with now: twice the base fee plus a priority fee, within the configured floor and cap.
    fn polygon_fees(&self) -> Result<(u128, u128), String> {
        let gwei = 1_000_000_000u128;
        let (base, suggested) = self.polygon.fees()?;
        let floor = u128::from(self.fees.priority_floor_gwei) * gwei;
        let cap = u128::from(self.fees.max_fee_cap_gwei) * gwei;
        let priority = suggested.clamp(floor, cap);
        let max_fee = base.saturating_mul(2).saturating_add(priority).min(cap);
        if max_fee < base.saturating_add(priority) {
            return Err(format!("Polygon's base fee of {} gwei is above the cap", base / gwei));
        }
        Ok((max_fee, priority))
    }

    fn sign_at(&self, nonce: u64, to: EvmAddress, value: u128, data: Vec<u8>, gas: u64, fees: (u128, u128)) -> Result<SignedCall, String> {
        let call = call_hash(&to, value, &data);
        let tx = Tx {
            chain_id: self.ids.chain_id,
            nonce,
            max_priority_fee_per_gas: fees.1,
            max_fee_per_gas: fees.0,
            gas,
            to,
            value,
            data,
        };
        let raw = tx.sign(&self.polygon_key)?;
        Ok(SignedCall { nonce, call, raw, max_fee: fees.0, priority_fee: fees.1 })
    }

    /// Sign a call, keeping the nonce rule (docs/design.md section 4.2). A call signed before keeps its nonce: the same bytes
    /// come back, unless the network's fees have risen past them, when it is signed again at a fee that outbids the old one.
    /// A new call gets the next nonce, and only if it would succeed now, so a call that can't land never holds a nonce.
    /// The caller commits the result before replying.
    fn sign_polygon(&self, to: EvmAddress, value: u128, data: Vec<u8>, gas: u64, simulate: bool) -> Result<SignedCall, Stop> {
        let call = call_hash(&to, value, &data);
        let mined = self.polygon.nonce(&self.me, "latest")?;
        if let Some(nonce) = self.ledger.nonce_of_call(&call).map_err(|e| e.to_string())? {
            let previous = self.ledger.nonce_call(nonce).map_err(|e| e.to_string())?.ok_or("a nonce's call is missing")?;
            return Ok(self.resign_if_needed(previous, mined, to, value, data, gas)?);
        }
        let nonce = self.ledger.next_nonce().map_err(|e| e.to_string())?.max(mined);
        policy::nonce_rule(self.ledger.nonce_call(nonce).map_err(|e| e.to_string())?.map(|s| s.call).as_ref(), &call)?;
        if simulate {
            self.polygon.simulate(&self.me, &to, &data, value).map_err(|e| format!("the call would fail now: {e}"))?;
        }
        let fees = self.polygon_fees()?;
        let needed = value.saturating_add(u128::from(gas).saturating_mul(fees.0));
        let held = self.polygon.balance(&self.me, "latest")?;
        if held < needed {
            return Err(Stop::Failed(format!("not enough POL: {held} wei held, {needed} needed")));
        }
        Ok(self.sign_at(nonce, to, value, data, gas, fees)?)
    }

    fn resign_if_needed(
        &self,
        previous: SignedCall,
        mined: u64,
        to: EvmAddress,
        value: u128,
        data: Vec<u8>,
        gas: u64,
    ) -> Result<SignedCall, String> {
        if previous.nonce < mined {
            return Ok(previous);
        }
        let (max_fee, priority) = self.polygon_fees()?;
        if max_fee <= previous.max_fee {
            return Ok(previous);
        }
        let cap = u128::from(self.fees.max_fee_cap_gwei) * 1_000_000_000;
        let fees = (max_fee.max(outbid(previous.max_fee)).min(cap), priority.max(outbid(previous.priority_fee)).min(cap));
        if fees.0 <= previous.max_fee {
            return Ok(previous);
        }
        self.sign_at(previous.nonce, to, value, data, gas, fees)
    }

    /// Every signed transaction not yet mined, each re-signed where the network's fees have passed it.
    fn pending_polygon(&self) -> Outcome {
        let mined = self.polygon.nonce(&self.me, "latest")?;
        let mut out = vec![];
        for previous in self.ledger.signed_from(mined).map_err(|e| e.to_string())? {
            let tx = decode_call(&previous.raw)?;
            let signed = self.resign_if_needed(previous.clone(), mined, tx.0, tx.1, tx.2, tx.3)?;
            if signed != previous {
                let swap = [0u8; 32];
                self.commit_polygon(&swap, &signed)?;
            }
            out.push(hex(&signed.raw));
        }
        Ok(Reply::PolygonPending(out))
    }

    fn approve_escrow(&self) -> Outcome {
        if self.vault.is_some() {
            return Err("with a maker vault there is nothing to approve: the vault approves each lock".to_string().into());
        }
        let none = [0u8; 32];
        let signed = self.sign_polygon(self.ids.token, 0, approve_call(&self.ids.escrow, u128::MAX), APPROVE_GAS, true)?;
        self.commit_polygon(&none, &signed)?;
        Ok(polygon_reply(&signed))
    }

    // ---- Liquid sessions ----

    /// L→P, step one. The taker's coin must already be good, so nobody can make us reserve coins for free.
    fn prepare_claim(&mut self, t: &SwapTerms) -> Outcome {
        let id = t.swap_id();
        if self.ledger.record(&id).adaptor_held {
            return Ok(Reply::Nothing);
        }
        if t.direction != Direction::L2P {
            return Err(Refusal::UnknownSwap.into());
        }
        // Asked again while waiting: the same offer, so an answer on its way still fits. Sending a nonce twice is safe, as
        // long as it signs once, and a session can only do that.
        if let Some(open) = self.sessions.get(&id).filter(|o| o.terms == *t) {
            let nonce = hex(&nonce_bytes(&open.session.public));
            return Ok(Reply::ClaimTemplate { tx: serialize_hex(&open.tx), nonce });
        }
        let tip = self.chain.tip()?;
        let coin = self.swap_coin(t)?.ok_or(Refusal::NoTakerCoin)?;
        let confirmations = coin.coin.height.map_or(0, |h| (tip + 1).saturating_sub(h));
        let needed =
            if t.polygon_amount > self.limits.large_above { self.limits.confirmations_large } else { self.limits.confirmations };
        if confirmations < needed {
            return Err(Refusal::NoTakerCoin.into());
        }
        let fee_coin = self.fee_coin_for(&id)?;
        let to = self.receive_address(Some(&id))?;
        let (tx, utxos) = liquid::build_swap_spend(&coin, &fee_coin, &to, 0, Sequence::MAX)?;
        let message = key_spend_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let key = self.swap_key(&id)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let session = Session::start(&key.secret_bytes(), output.key_ctx(), &message).map_err(|e| e.to_string())?;
        let reply = Reply::ClaimTemplate { tx: serialize_hex(&tx), nonce: hex(&nonce_bytes(&session.public)) };
        // Replaces any earlier session, whose nonce is dropped unused.
        self.sessions.insert(id, OpenSession { terms: t.clone(), tx, message, session, their_nonce: None });
        Ok(reply)
    }

    /// Take the open session for these terms. It is used once, whatever happens next, so its nonce can't sign twice.
    fn take_session(&mut self, t: &SwapTerms) -> Result<OpenSession, Stop> {
        let id = t.swap_id();
        let open = self.sessions.remove(&id).ok_or("no signing session is open for this swap")?;
        if open.terms != *t {
            return Err("these terms differ from the session's".into());
        }
        Ok(open)
    }

    /// L→P, step two. Our own share stays here, so the taker gets nothing from this.
    fn store_adaptor(&mut self, t: &SwapTerms, taker_nonce: &str, taker_share: &str) -> Outcome {
        let id = t.swap_id();
        let request = Request::HoldAdaptor { swap_id: id };
        let record = self.check(&request)?;
        let open = self.take_session(t)?;
        let their_nonce = nonce_from(&unhex(taker_nonce)?).map_err(|e| e.to_string())?;
        let theirs = share_from(&unhex(taker_share)?).map_err(|e| e.to_string())?;
        let key = self.swap_key(&id)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let ctx = output.key_ctx();
        let (ours, aggregate) =
            open.session.sign(&key.secret_bytes(), ctx, &their_nonce, &t.lock_point.0, &open.message).map_err(|e| e.to_string())?;
        verify_share(ctx, theirs, &aggregate, &t.taker_key.0, &their_nonce, &t.lock_point.0, &open.message)
            .map_err(|e| e.to_string())?;
        let adaptor = combine(ctx, &aggregate, [ours, theirs], &t.lock_point.0, &open.message).map_err(|e| e.to_string())?;
        let held = HeldAdaptor { adaptor: hex(&adaptor_bytes(&adaptor)), tx: serialize_hex(&open.tx), share: None };
        self.keep_adaptor(&id, &record, &held)?;
        Ok(Reply::Held(true))
    }

    fn keep_adaptor(&self, id: &[u8; 32], record: &policy::SwapRecord, held: &HeldAdaptor) -> Result<(), Stop> {
        let body = serde_json::to_string(held).map_err(|e| e.to_string())?;
        self.ledger.commit_adaptor(id, record, &body).map_err(|e| Stop::Failed(e.to_string()))
    }

    fn held_adaptor(&self, id: &[u8; 32]) -> Result<Option<HeldAdaptor>, Stop> {
        let body = self.ledger.adaptor(id).map_err(|e| e.to_string())?;
        Ok(body.map(|b| serde_json::from_str(&b)).transpose().map_err(|e| e.to_string())?)
    }

    /// The same promise an early refund makes, made before the taker has what the refund needs.
    fn cancel(&mut self, t: &SwapTerms) -> Outcome {
        let id = t.swap_id();
        if t.direction != Direction::L2P {
            return Err(Refusal::UnknownSwap.into());
        }
        let request = Request::EarlyRefund { swap_id: id };
        let record = self.check(&request)?;
        self.commit(&id, &request, &record, None, None)?;
        self.sessions.remove(&id);
        Ok(Reply::Nothing)
    }

    /// L→P: agree to the taker's refund. We sign only input 0, with this swap's own key, so nothing else of ours can be
    /// spent, and where the coin goes is the taker's business. The refusal to ever lock is durable before the share leaves.
    /// A fresh nonce each time, because the taker may lose its session and ask again.
    fn early_refund(&mut self, t: &SwapTerms, tx: &str, taker_nonce: &str) -> Outcome {
        let id = t.swap_id();
        if t.direction != Direction::L2P {
            return Err(Refusal::UnknownSwap.into());
        }
        let request = Request::EarlyRefund { swap_id: id };
        let record = self.check(&request)?;
        let tx: LiquidTx = deserialize(&unhex(tx)?).map_err(|e| e.to_string())?;
        // Any coin the taker sent here, the swap's own or one of the wrong asset or amount: we never lock for a wrong one,
        // so it is the taker's either way. Our signature covers the coin it spends, so it can't move any other.
        let address = t.address(&self.maker_key(&id)?)?;
        let coins = self.chain.secret_coins(&address, &t.blinding()?)?;
        if coins.is_empty() {
            return Err(Refusal::NoTakerCoin.into());
        }
        let first = tx.input.first().map(|i| i.previous_output);
        if !coins.iter().any(|c| Some(c.coin.outpoint) == first) {
            return Err("input 0 doesn't spend a coin at this swap's address".into());
        }
        // From the chain, so the message is what the network will check, not what the taker says it is.
        let utxos = spent_outputs(&self.chain, &tx)?;
        let message = key_spend_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let their_nonce = nonce_from(&unhex(taker_nonce)?).map_err(|e| e.to_string())?;
        let key = self.swap_key(&id)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let session = Session::start(&key.secret_bytes(), output.key_ctx(), &message).map_err(|e| e.to_string())?;
        let nonce = hex(&nonce_bytes(&session.public));
        let (share, _) =
            session.sign_plain(&key.secret_bytes(), output.key_ctx(), &their_nonce, &message).map_err(|e| e.to_string())?;
        self.commit(&id, &request, &record, None, None)?;
        // A claim session still open could only ever be refused now.
        self.sessions.remove(&id);
        Ok(Reply::EarlyRefundShare { nonce, share: hex(&share_bytes(&share)) })
    }

    // ---- Locks ----

    fn lock_polygon(&self, t: &SwapTerms, deadline_time: u64, deadline_block: u64) -> Outcome {
        if t.direction != Direction::L2P {
            return Err(Refusal::UnknownSwap.into());
        }
        let taker = t.taker()?;
        let gas_to = match (&t.gas_to, t.gas_drop) {
            (_, 0) => EvmAddress::ZERO,
            (Some(a), _) => EvmAddress::parse(a).map_err(|e| format!("bad gas_to: {e}"))?,
            (None, _) => return Err("a gas drop needs gas_to".into()),
        };
        let tip = self.chain.tip()?;
        let taker_coin = self.swap_coin(t)?.map(|c| CoinEvidence {
            amount: c.secrets.value,
            confirmations: c.coin.height.map_or(0, |h| (tip + 1).saturating_sub(h)),
        });
        let id = t.swap_id();
        let request = Request::LockPolygon {
            swap_id: id,
            amount: t.polygon_amount,
            deadline_time,
            deadline_block,
            gas_drop: t.gas_drop,
            clock: self.polygon_clock()?,
            liquid_lock_height: t.lock_height,
            liquid_tip: tip,
            taker_coin,
            recipient_blocked: self.polygon.is_blocked(&self.ids.token, &taker, "latest")?,
        };
        let record = self.check(&request)?;
        let lock_address = lock_address(&t.lock_point)?;
        let amount = u128::from(t.polygon_amount);
        let (to, data, gas) = match self.vault {
            // The vault checks its own cap; the simulation below refuses a lock over it.
            Some(vault) => {
                (vault, vault_lock_call(&lock_address, &taker, amount, deadline_time, deadline_block, &gas_to), VAULT_LOCK_GAS)
            }
            None => {
                let allowance = self.polygon.allowance(&self.ids.token, &self.me, &self.ids.escrow)?;
                if allowance < amount {
                    return Err("the escrow isn't approved for our USDT0: call ApproveEscrow".into());
                }
                let data = lock_call(&lock_address, &taker, amount, deadline_time, deadline_block, false, &gas_to);
                (self.ids.escrow, data, LOCK_GAS)
            }
        };
        let signed = self.sign_polygon(to, t.gas_drop, data, gas, true)?;
        self.commit(&id, &request, &record, None, Some(&signed))?;
        Ok(polygon_reply(&signed))
    }

    /// The taker's P→L lock, as our own connection reads it at `finalized`, with how it compares to the terms.
    fn taker_lock(&self, taker_polygon: &str, lock_point: &LockPoint) -> Result<(polygon::Lock, [u8; 32], bool), String> {
        let taker = EvmAddress::parse(taker_polygon).map_err(|e| format!("bad taker address: {e}"))?;
        let address = lock_address(lock_point)?;
        let id = lock_id(&taker, &address);
        let lock = self.polygon.lock_at(&self.ids.escrow, &id, "finalized")?;
        let ours =
            lock.state == LockState::Open && lock.lock_address == address && lock.recipient == self.holder() && lock.long_leg;
        Ok((lock, id, ours))
    }

    fn lock_liquid(&self, t: &SwapTerms) -> Outcome {
        let id = t.swap_id();
        let address = t.address(&self.maker_key(&id)?)?;

        // A retry gets the same bytes, so the address can never be paid twice. Different terms get nothing.
        if let Some(raw) = self.ledger.lock_tx(&id).map_err(|e| e.to_string())? {
            let tx: LiquidTx = deserialize(&raw).map_err(|e| e.to_string())?;
            if tx.output.first().map(|o| &o.script_pubkey) != Some(&address.script_pubkey()) {
                return Err(Refusal::AlreadyLocked.into());
            }
            return Ok(Reply::Liquid(hex(&raw)));
        }
        if t.direction != Direction::P2L {
            return Err(Refusal::UnknownSwap.into());
        }
        let clock = self.polygon_clock()?;
        let (lock, _, ours) = self.taker_lock(&t.taker_polygon, &t.lock_point)?;
        let taker_escrow = (lock.state != LockState::None).then(|| {
            let (secs_left_time, secs_left_blocks) = secs_left(lock.deadline_time, lock.deadline_block, &clock);
            EscrowEvidence {
                amount: u64::try_from(lock.amount).unwrap_or(0),
                matches_terms: ours && !lock.extended,
                secs_left_time,
                secs_left_blocks,
            }
        });
        let request = Request::LockLiquid {
            swap_id: id,
            liquid_amount: t.liquid_amount,
            lock_height: t.lock_height,
            liquid_tip: self.chain.tip()?,
            taker_escrow,
            maker_blocked: self.polygon.is_blocked(&self.ids.token, &self.holder(), "latest")?,
        };
        let record = self.check(&request)?;
        let payments = vec![
            Payment { asset: usdt(), amount: t.liquid_amount, address },
            // Exactly one fee coin's worth of L-BTC, so this can't be used to drain the wallet's L-BTC.
            Payment { asset: liquid::lbtc(), amount: self.fee_coin_sats, address: t.taker_fee_address()? },
        ];
        let tx = self.pay(payments, &self.receive_address(Some(&id))?)?;
        let raw = serialize(&tx);
        self.commit(&id, &request, &record, Some((&raw, &spends_of(&tx))), None)?;
        Ok(Reply::Liquid(hex(&raw)))
    }

    /// P→L, step one. The taker chooses where its claim pays, because the coin becomes the taker's.
    fn open_session(&mut self, t: &SwapTerms, claim_tx: &str, taker_nonce: &str) -> Outcome {
        let id = t.swap_id();
        if self.ledger.record(&id).direction != Some(Direction::P2L) {
            return Err(Refusal::UnknownSwap.into());
        }
        if self.ledger.record(&id).adaptor_held {
            return Err(Refusal::AdaptorAlreadyHeld.into());
        }
        let tx: LiquidTx = deserialize(&unhex(claim_tx)?).map_err(|e| e.to_string())?;
        let coin = self.swap_coin(t)?.ok_or("our coin isn't there")?;
        check_claim_shape(&tx, &coin.coin.outpoint)?;
        // From the chain, so the message is what the network will check, not what the taker says it is.
        let utxos = spent_outputs(&self.chain, &tx)?;
        let message = key_spend_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let their_nonce = nonce_from(&unhex(taker_nonce)?).map_err(|e| e.to_string())?;
        let key = self.swap_key(&id)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let session = Session::start(&key.secret_bytes(), output.key_ctx(), &message).map_err(|e| e.to_string())?;
        let reply = Reply::Nonce(hex(&nonce_bytes(&session.public)));
        self.sessions.insert(id, OpenSession { terms: t.clone(), tx, message, session, their_nonce: Some(their_nonce) });
        Ok(reply)
    }

    /// P→L, step two. The order is what keeps us safe: with our share the taker can finish the claim, and without the
    /// stored adapter signature we couldn't read the secret from it. So the signature goes in the ledger first.
    fn finish_session(&mut self, t: &SwapTerms, taker_share: &str) -> Outcome {
        let id = t.swap_id();
        if let Some(HeldAdaptor { share: Some(share), .. }) = self.held_adaptor(&id)? {
            return Ok(Reply::Share(share));
        }
        let clock = self.polygon_clock()?;
        let (lock, _, ours) = self.taker_lock(&t.taker_polygon, &t.lock_point)?;
        let taker_escrow_open = ours && lock.can_claim(clock.time, clock.block);
        let request =
            Request::GiveShare { swap_id: id, lock_height: t.lock_height, liquid_tip: self.chain.tip()?, taker_escrow_open };
        let record = self.check(&request)?;
        let open = self.take_session(t)?;
        let their_nonce = open.their_nonce.ok_or("this session has no taker nonce")?;
        let theirs = share_from(&unhex(taker_share)?).map_err(|e| e.to_string())?;
        let key = self.swap_key(&id)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let ctx = output.key_ctx();
        let (ours, aggregate) =
            open.session.sign(&key.secret_bytes(), ctx, &their_nonce, &t.lock_point.0, &open.message).map_err(|e| e.to_string())?;
        verify_share(ctx, theirs, &aggregate, &t.taker_key.0, &their_nonce, &t.lock_point.0, &open.message)
            .map_err(|e| e.to_string())?;
        let adaptor = combine(ctx, &aggregate, [ours, theirs], &t.lock_point.0, &open.message).map_err(|e| e.to_string())?;
        let share = hex(&share_bytes(&ours));
        let held = HeldAdaptor { adaptor: hex(&adaptor_bytes(&adaptor)), tx: serialize_hex(&open.tx), share: Some(share.clone()) };
        self.keep_adaptor(&id, &record, &held)?;
        Ok(Reply::Share(share))
    }

    fn secret_from_signature(&self, lock_point: &LockPoint, signature: &str) -> Outcome {
        let held = self.held_adaptor(&lock_point.id())?.ok_or("no adapter signature is stored for this swap")?;
        let adaptor = adaptor_from(&unhex(&held.adaptor)?).map_err(|e| e.to_string())?;
        let published: [u8; 64] = unhex(signature)?.try_into().map_err(|_| "a signature is 64 bytes")?;
        let secret = extract_secret(&adaptor, &published, &lock_point.0).ok_or("that signature doesn't give the secret")?;
        Ok(Reply::Secret(secret))
    }

    // ---- Claims and refunds ----

    fn claim_liquid(&self, t: &SwapTerms, secret: [u8; 32]) -> Outcome {
        let id = t.swap_id();
        let request = Request::ClaimLiquid { lock_point: t.lock_point, secret };
        let record = self.check(&request)?;
        let held = self.held_adaptor(&id)?.ok_or("no adapter signature is stored for this swap")?;
        let adaptor = adaptor_from(&unhex(&held.adaptor)?).map_err(|e| e.to_string())?;
        let mut tx: LiquidTx = deserialize(&unhex(&held.tx)?).map_err(|e| e.to_string())?;
        let utxos = spent_outputs(&self.chain, &tx)?;
        let signature = complete(&adaptor, &secret).map_err(|e| e.to_string())?;
        set_key_spend_witness(&mut tx, 0, &signature);
        self.sign_fee_coin(&mut tx, &utxos)?;
        self.commit(&id, &request, &record, None, None)?;
        Ok(Reply::Liquid(serialize_hex(&tx)))
    }

    /// P→L: the claim pays its recipient, which the taker's lock fixed as us, whoever sends it.
    fn claim_polygon(&self, taker_polygon: &str, lock_point: LockPoint, secret: [u8; 32]) -> Outcome {
        let request = Request::ClaimPolygon { lock_point, secret };
        let record = self.check(&request)?;
        let (lock, id, _) = self.taker_lock(taker_polygon, &lock_point)?;
        if lock.recipient != self.holder() {
            return Err("that lock doesn't pay us".into());
        }
        let revealed = polygon_value(&secret).map_err(|e| e.to_string())?;
        // No simulation: a claim must go out even when a read lags, and one that fails only costs its gas.
        let signed = self.sign_polygon(self.ids.escrow, 0, claim_call(&id, &revealed), CLAIM_GAS, false)?;
        // Recorded before release: from here on, the Liquid coin is the taker's, even if this is never sent.
        self.commit(&lock_point.id(), &request, &record, None, Some(&signed))?;
        Ok(polygon_reply(&signed))
    }

    fn refund_liquid(&self, t: &SwapTerms) -> Outcome {
        let id = t.swap_id();
        let request = Request::RefundLiquid { swap_id: id, lock_height: t.lock_height, liquid_tip: self.chain.tip()? };
        let record = self.check(&request)?;
        let coin = self.swap_coin(t)?.ok_or("our coin isn't there")?;
        let fee_coin = self.fee_coin_for(&id)?;
        let to = self.receive_address(Some(&id))?;
        let (tx, utxos) = liquid::build_swap_spend(&coin, &fee_coin, &to, t.lock_height, Sequence::ZERO)?;
        let output = t.output(&self.maker_key(&id)?)?;
        let digest = output.refund_sighash(&tx, &utxos, 0, self.genesis).map_err(|e| e.to_string())?;
        let secp = Secp256k1::new();
        let key = self.swap_key(&id)?;
        let signature = *secp.sign_schnorr_no_aux_rand(&Message::from_digest(digest), &key).as_ref();
        let mut tx = output.finalize_refund(tx, &utxos, 0, self.genesis, signature).map_err(|e| e.to_string())?;
        self.sign_fee_coin(&mut tx, &utxos)?;
        self.commit(&id, &request, &record, None, None)?;
        Ok(Reply::Liquid(serialize_hex(&tx)))
    }

    /// L→P: our own lock back, to us, which the escrow fixes. Only once the taker's claim has closed, which the escrow checks.
    fn refund_polygon(&self, lock_point: LockPoint) -> Outcome {
        let request = Request::RefundPolygon { swap_id: lock_point.id() };
        let record = self.check(&request)?;
        let id = lock_id(&self.holder(), &lock_address(&lock_point)?);
        let signed = self.sign_polygon(self.ids.escrow, 0, refund_call(&id), REFUND_GAS, true)?;
        self.commit(&lock_point.id(), &request, &record, None, Some(&signed))?;
        Ok(polygon_reply(&signed))
    }

    fn extend(&self, taker_polygon: &str, lock_point: LockPoint, new_time: u64, new_block: u64) -> Outcome {
        let (lock, id, ours) = self.taker_lock(taker_polygon, &lock_point)?;
        if !ours {
            return Err("that lock isn't an open long leg paying us".into());
        }
        let request = Request::Extend {
            swap_id: lock_point.id(),
            current_time: lock.deadline_time,
            current_block: lock.deadline_block,
            new_time,
            new_block,
            liquid_tip_age_secs: now_secs().saturating_sub(self.chain.tip_time()?),
        };
        let record = self.check(&request)?;
        // The escrow takes `extend` only from the lock's recipient: the vault, when there is one, passes ours on.
        let to = self.vault.unwrap_or(self.ids.escrow);
        let signed = self.sign_polygon(to, 0, extend_call(&id, new_time, new_block), EXTEND_GAS, true)?;
        self.commit(&lock_point.id(), &request, &record, None, Some(&signed))?;
        Ok(polygon_reply(&signed))
    }

    /// P→L: hand the taker's lock back. The escrow pays only its initiator, so we can only say yes or no.
    fn release(&self, taker_polygon: &str, lock_point: LockPoint) -> Outcome {
        let request = Request::Release { swap_id: lock_point.id() };
        let record = self.check(&request)?;
        let (_, id, _) = self.taker_lock(taker_polygon, &lock_point)?;
        let to = self.vault.unwrap_or(self.ids.escrow);
        let signed = self.sign_polygon(to, 0, release_call(&id), RELEASE_GAS, true)?;
        self.commit(&lock_point.id(), &request, &record, None, Some(&signed))?;
        Ok(polygon_reply(&signed))
    }

    pub fn handle(&mut self, call: &Call) -> Reply {
        let outcome: Outcome = match call {
            Call::Identity => Ok(Reply::Identity {
                liquid_address: self.wallet.address().to_string(),
                polygon: self.holder().to_string(),
                fee_coin_sats: self.fee_coin_sats,
            }),
            Call::SwapKey { swap_id } => self.maker_key(swap_id).map(|key| Reply::SwapKey { key }).map_err(Stop::Failed),
            Call::AdaptorHeld { swap_id } => Ok(Reply::Held(self.ledger.record(swap_id).adaptor_held)),
            Call::TopUpFeeCoins => self.top_up_fee_coins(),
            Call::ApproveEscrow => self.approve_escrow(),
            Call::Capacity => self.capacity(),
            Call::Balances => self.balances(),
            Call::PrepareClaim { terms } => self.prepare_claim(terms),
            Call::StoreAdaptor { terms, taker_nonce, taker_share } => self.store_adaptor(terms, taker_nonce, taker_share),
            Call::Cancel { terms } => self.cancel(terms),
            Call::EarlyRefund { terms, tx, taker_nonce } => self.early_refund(terms, tx, taker_nonce),
            Call::LockPolygon { terms, deadline_time, deadline_block } => {
                self.lock_polygon(terms, *deadline_time, *deadline_block)
            }
            Call::LockLiquid { terms } => self.lock_liquid(terms),
            Call::OpenSession { terms, claim_tx, taker_nonce } => self.open_session(terms, claim_tx, taker_nonce),
            Call::FinishSession { terms, taker_share } => self.finish_session(terms, taker_share),
            Call::ExtractSecret { lock_point, signature } => self.secret_from_signature(lock_point, signature),
            Call::ClaimLiquid { terms, secret } => self.claim_liquid(terms, *secret),
            Call::ClaimPolygon { taker_polygon, lock_point, secret } => self.claim_polygon(taker_polygon, *lock_point, *secret),
            Call::RefundLiquid { terms } => self.refund_liquid(terms),
            Call::RefundPolygon { lock_point } => self.refund_polygon(*lock_point),
            Call::Withdraw { chain: Chain::Liquid, asset, amount } => self.withdraw_liquid(*asset, *amount),
            Call::Withdraw { chain: Chain::Polygon, asset, amount } => self.withdraw_polygon(*asset, *amount),
            Call::Extend { taker_polygon, lock_point, new_time, new_block } => {
                self.extend(taker_polygon, *lock_point, *new_time, *new_block)
            }
            Call::Release { taker_polygon, lock_point } => self.release(taker_polygon, *lock_point),
            Call::PendingPolygon => self.pending_polygon(),
        };
        match outcome {
            Ok(reply) => reply,
            Err(Stop::Refused(r)) => Reply::Refused(r),
            Err(Stop::Failed(e)) => Reply::Failed(e),
        }
    }
}

/// A maker vault we may use: for our escrow, with our key as its operator, and a window it can lock in at all.
fn check_vault(state: &polygon::VaultState, ids: &polygon::Ids, me: &EvmAddress) -> Result<(), String> {
    if state.escrow != ids.escrow {
        return Err(format!("refusing to start: the maker vault is for escrow {}, not ours ({})", state.escrow, ids.escrow));
    }
    if state.operator != *me {
        return Err(format!("refusing to start: the maker vault's operator is {}, not our key ({me})", state.operator));
    }
    if state.window_cap == 0 {
        return Err("refusing to start: the maker vault's cap is zero, so it can't lock anything".into());
    }
    Ok(())
}

fn polygon_reply(signed: &SignedCall) -> Reply {
    Reply::Polygon { raw: hex(&signed.raw), nonce: signed.nonce, hash: hex(&evm::tx_hash(&signed.raw)) }
}

/// The destination, value, data and gas of one of our own signed transactions, to sign it again at a new fee.
fn decode_call(raw: &[u8]) -> Result<(EvmAddress, u128, Vec<u8>, u64), String> {
    let tx = evm::decode_signed(raw)?;
    Ok((tx.to, tx.value, tx.data, tx.gas))
}

fn pol_kept(unsettled: u64) -> u128 {
    u128::from(unsettled).saturating_mul(POL_KEPT_PER_P2L_LOCK)
}

/// Whether a POL withdrawal leaves enough for the P→L swaps still to claim on Polygon.
fn pol_left_is_enough(unsettled: u64, left: u128) -> Result<(), String> {
    let keep = pol_kept(unsettled);
    if unsettled > 0 && left < keep {
        return Err(format!(
            "refusing: {unsettled} P→L swaps are locked on Liquid and still need POL to claim on Polygon. Keep at least {:.2} POL until they settle.",
            keep as f64 / 1e18
        ));
    }
    Ok(())
}

/// A withdrawal address must be confidential, so the amount stays hidden, and on this network, or the coins would be lost.
fn withdrawal_address(s: &str) -> Result<Address, String> {
    let address = Address::from_str(s).map_err(|e| format!("refusing to start: withdraw_to.liquid isn't a Liquid address: {e}"))?;
    if address.params != liquid::network().params {
        return Err("refusing to start: withdraw_to.liquid is for another network".into());
    }
    if !address.is_blinded() {
        return Err("refusing to start: withdraw_to.liquid must be a confidential address".into());
    }
    Ok(address)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Serve one connection until it closes. A bad or oversized request gets an error, and nothing else happens.
/// Requests are read before taking the lock, so a slow or stalled client can never hold up anyone else.
fn serve(signer: &Mutex<Signer>, stream: UnixStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader).take(MAX_REQUEST_BYTES + 1).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        if line.len() as u64 > MAX_REQUEST_BYTES {
            let reply = Reply::Failed("request too large".into());
            writeln!(writer, "{}", serde_json::to_string(&reply).unwrap_or_default())?;
            return Ok(());
        }
        let reply = match serde_json::from_slice::<Call>(&line) {
            // One request at a time, so policy checks can't race. A poisoned lock means a request panicked part way,
            // so stop signing rather than carry on from an unknown state.
            Ok(call) => signer.lock().map_err(|_| std::io::Error::other("signer poisoned"))?.handle(&call),
            Err(_) => Reply::Failed("malformed request".into()),
        };
        writeln!(writer, "{}", serde_json::to_string(&reply).unwrap_or_default())?;
    }
}

/// Paths in a config are relative to the config file, so a maker's folder can be copied without pointing at the original.
fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut cfg: Config = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let base = path.parent().unwrap_or(Path::new("."));
    // An absolute path is kept as it is, because joining onto one replaces what came before.
    cfg.socket = base.join(&cfg.socket);
    cfg.ledger = base.join(&cfg.ledger);
    cfg.keys_dir = base.join(&cfg.keys_dir);
    Ok(cfg)
}

/// Run until the process is stopped.
pub fn run(config_path: &Path) -> Result<(), String> {
    let cfg = load_config(config_path)?;
    // A chain outage must not stop us coming back up, or we couldn't sign claims when it ends. The mainnet check stays
    // fail-closed: nothing is served until it has actually passed.
    let signer = loop {
        match Signer::new(&cfg) {
            Ok(signer) => break Arc::new(Mutex::new(signer)),
            Err(e) if e.starts_with("refusing") => return Err(e),
            Err(e) => {
                eprintln!("waiting for the chains: {e}");
                // Not faster than this: if a public API is rate limiting us, retrying only prolongs it.
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
    };
    // Public addresses only, so an operator with new keys knows where to send funds.
    if let Ok(mut guard) = signer.lock() {
        let key = guard.me;
        if let Reply::Identity { liquid_address, polygon, .. } = guard.handle(&Call::Identity) {
            if polygon == key.to_string() {
                println!("signer ready. To fund it: Liquid {liquid_address}, Polygon {polygon}");
            } else {
                // With a maker vault, its owner funds the vault, and the key needs only POL, for gas and gas drops.
                println!("signer ready. To fund it: Liquid {liquid_address}, Polygon {polygon} (the vault), POL for gas {key}");
            }
        }
    }
    let _ = std::fs::remove_file(&cfg.socket);
    // The socket's own mode is set just after binding, so the directory must already keep everyone else out.
    let parent = cfg.socket.parent().ok_or("socket has no parent directory")?;
    if !parent.exists() {
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(parent)
            .map_err(|e| format!("can't create {}: {e}", parent.display()))?;
    }
    let mode = std::fs::metadata(parent).map_err(|e| e.to_string())?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!("refusing to start: {} must be private to this user (mode 700)", parent.display()));
    }
    let listener = UnixListener::bind(&cfg.socket).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            continue; // Dropping the stream closes it.
        }
        let (signer, open) = (signer.clone(), open.clone());
        std::thread::spawn(move || {
            if let Err(e) = serve(&signer, stream) {
                eprintln!("connection ended: {e}");
            }
            open.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// The engine's end of the socket. A connection the signer closed, as it does when it restarts, is found when writing to it
/// fails: the request never reached the signer, so it goes again, once, on a new connection. A failure after the request was
/// written is returned, and the next call connects again, so a caller that keeps a `Client` recovers on its own. A caller that
/// tries such a call again is safe too: the signer answers a repeated request with the same signed bytes.
pub struct Client {
    path: PathBuf,
    conn: Option<(UnixStream, BufReader<UnixStream>)>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, String> {
        let mut client = Self { path: path.to_path_buf(), conn: None };
        client.open()?;
        Ok(client)
    }

    fn open(&mut self) -> Result<&mut (UnixStream, BufReader<UnixStream>), String> {
        if self.conn.is_none() {
            let writer = UnixStream::connect(&self.path)
                .map_err(|e| format!("can't reach the signer at {}: {e}", self.path.display()))?;
            let reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
            self.conn = Some((writer, reader));
        }
        Ok(self.conn.as_mut().expect("just opened"))
    }

    /// Write `bytes` and read one line back. Any failure, or the signer closing the connection, drops it.
    fn exchange(&mut self, bytes: &[u8]) -> Result<String, String> {
        let written = self.open()?.0.write_all(bytes);
        if written.is_err() {
            // Not delivered: a closed Unix socket refuses the write itself. Once more, on a new connection.
            self.conn = None;
            self.open()?.0.write_all(bytes).map_err(|e| {
                self.conn = None;
                e.to_string()
            })?;
        }
        let (_, reader) = self.conn.as_mut().expect("open");
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                self.conn = None;
                Err("the signer closed the connection".into())
            }
            Ok(_) => Ok(line),
            Err(e) => {
                self.conn = None;
                Err(e.to_string())
            }
        }
    }

    pub fn call(&mut self, call: &Call) -> Result<Reply, String> {
        let mut text = serde_json::to_string(call).map_err(|e| e.to_string())?;
        text.push('\n');
        let line = self.exchange(text.as_bytes())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    }

    /// Send raw bytes, for testing how the signer handles bad input.
    pub fn call_raw(&mut self, bytes: &[u8]) -> Result<String, String> {
        self.exchange(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signer that restarts drops every connection. The client's next call must reach the new one, not fail for ever.
    #[test]
    fn a_client_reconnects_after_the_signer_restarts() {
        use std::io::BufRead;
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("liq2any-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (closed, wait_closed) = std::sync::mpsc::channel();
        // Each connection answers one request, then closes, as a restarting signer does.
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut w = stream;
                writeln!(w, "{}", serde_json::to_string(&Reply::Held(true)).unwrap()).unwrap();
                drop((w, reader));
                closed.send(()).unwrap();
            }
        });
        let mut client = Client::connect(&path).unwrap();
        let ask = Call::AdaptorHeld { swap_id: [0; 32] };
        assert_eq!(client.call(&ask), Ok(Reply::Held(true)));
        wait_closed.recv().unwrap();
        // The first connection is gone. The write finds that out, so the call goes again on a new one.
        assert_eq!(client.call(&ask), Ok(Reply::Held(true)), "a call after the signer restarted reaches the new one");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_our_own_vault_is_used() {
        let me = EvmAddress([2; 20]);
        let ids = polygon::Ids { chain_id: 80002, escrow: EvmAddress([1; 20]), token: EvmAddress([9; 20]) };
        let good = polygon::VaultState {
            escrow: ids.escrow,
            operator: me,
            window_secs: 86_400,
            window_cap: 1,
            window_start: 0,
            window_used: 0,
        };
        assert_eq!(check_vault(&good, &ids, &me), Ok(()));
        let other_escrow = polygon::VaultState { escrow: EvmAddress([3; 20]), ..good };
        assert!(check_vault(&other_escrow, &ids, &me).unwrap_err().contains("not ours"));
        let other_operator = polygon::VaultState { operator: EvmAddress([4; 20]), ..good };
        assert!(check_vault(&other_operator, &ids, &me).unwrap_err().contains("operator is"));
        let no_cap = polygon::VaultState { window_cap: 0, ..good };
        assert!(check_vault(&no_cap, &ids, &me).unwrap_err().contains("cap is zero"));
    }

    #[test]
    fn the_example_config_stays_inside_the_folder_it_is_copied_to() {
        let dir = std::env::temp_dir().join(format!("liq2any-signer-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("signer.json");
        std::fs::write(&path, include_str!("../../../deploy/signer.example.json")).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.socket, dir.join("run/signer.sock"));
        assert_eq!(cfg.ledger, dir.join("signer.db"));
        assert_eq!(cfg.keys_dir, dir.join("keys"));
        assert_eq!(cfg.polygon.escrow.as_deref(), Some("0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F"), "Amoy's");
    }

    #[test]
    fn a_pol_withdrawal_leaves_enough_for_p2l_claims() {
        assert!(pol_left_is_enough(0, 0).is_ok(), "with nothing to claim, all of it may go");
        assert!(pol_left_is_enough(1, POL_KEPT_PER_P2L_LOCK).is_ok());
        assert!(pol_left_is_enough(1, POL_KEPT_PER_P2L_LOCK - 1).is_err(), "less than a claim's reserve is refused");
        assert!(pol_left_is_enough(2, POL_KEPT_PER_P2L_LOCK).is_err(), "a reserve for each swap");
        assert!(pol_left_is_enough(u64::MAX, pol_kept(u64::MAX) - 1).is_err(), "and the reserve scales with every swap");
    }

    #[test]
    fn a_replacement_outbids_by_more_than_ten_percent() {
        assert_eq!(outbid(100), 113);
        assert_eq!(outbid(0), 1);
        assert_eq!(outbid(u128::MAX), u128::MAX, "no overflow");
        for fee in [1u128, 7, 1_000_000_000, 30_000_000_000_000] {
            assert!(outbid(fee) * 10 > fee * 11, "{fee}");
        }
    }

    #[test]
    fn a_call_is_identified_by_destination_value_and_data() {
        let (a, b) = (EvmAddress([1; 20]), EvmAddress([2; 20]));
        let base = call_hash(&a, 5, b"data");
        assert_eq!(base, call_hash(&a, 5, b"data"));
        assert_ne!(base, call_hash(&b, 5, b"data"));
        assert_ne!(base, call_hash(&a, 6, b"data"));
        assert_ne!(base, call_hash(&a, 5, b"datb"));
    }

    #[test]
    fn fresh_addresses_derive_apart_and_the_same_every_time() {
        let secrets = ([7u8; 32], [9u8; 32]);
        let zero = wallet_at(&secrets, 0).unwrap();
        assert_eq!(zero.key, keypair_from(&secrets.0).unwrap(), "index 0 is the key files themselves");
        assert_eq!(zero.blinding.secret_bytes(), secrets.1);
        let addresses: Vec<String> = (0..20).map(|i| wallet_at(&secrets, i).unwrap().address().to_string()).collect();
        let mut unique = addresses.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), addresses.len(), "every index has its own address");
        assert_eq!(wallet_at(&secrets, 5).unwrap().address(), wallet_at(&secrets, 5).unwrap().address(), "and the same one each time");
        let one = wallet_at(&secrets, 1).unwrap();
        assert_ne!(one.blinding.secret_bytes(), secrets.1, "each index has its own blinding key too");
    }

    #[test]
    fn no_wire_type_can_carry_a_key() {
        // A reply can only be one of these, and none holds a key or a secret nonce: this match fails to compile if one is
        // added. `Secret` carries only a swap secret the taker has already made public.
        let reply = Reply::Failed(String::new());
        match reply {
            Reply::Identity { .. }
            | Reply::SwapKey { .. }
            | Reply::Held(_)
            | Reply::Capacity { .. }
            | Reply::Balances { .. }
            | Reply::Nothing
            | Reply::ClaimTemplate { .. }
            | Reply::Nonce(_)
            | Reply::Share(_)
            | Reply::EarlyRefundShare { .. }
            | Reply::Secret(_)
            | Reply::Liquid(_)
            | Reply::EarlierWithdrawal(_)
            | Reply::Polygon { .. }
            | Reply::PolygonPending(_)
            | Reply::Refused(_)
            | Reply::Failed(_) => {}
        }
    }
}
