//! The real daemon and signer processes, against a regtest Liquid node and a local `anvil` chain with the real escrow and the
//! stand-in token, with a simulated taker. From liq2sol's suite (docs/copied.md), with Polygon in Solana's place, and three
//! scenarios of its own: a Polygon finality stall, blocked addresses, and the gas drop. Each scenario checks where the money
//! ended up. A swap's taker is simulated by a bot answering over the relay, as a real wallet would.
//!
//!   cargo build && ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli \
//!     cargo test -p liq2any-maker --test e2e -- --ignored --nocapture
//!
//! `cargo build` first, because this starts the signer binary from the same workspace. Needs Foundry (`anvil`, `forge`,
//! `cast`) on the PATH. Anvil mines a block a second and finalizes two behind the head, close to Polygon's own finality.
//! Test networks only: anvil's well-known keys hold nothing real. `SCENARIO` runs a subset.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use liq2any_chain::esplora::{build_confidential_pay, Esplora, Rpc};
use liq2any_chain::evm::{from_hex, Address as EvmAddress};
use liq2any_chain::liquid::{
    build_swap_spend, exact_secret_coin, keypair_from, lbtc, random32, sign_keypath, usdt, use_network, NetworkConfig,
    Payment, SecretCoin, Wallet, FEE,
};
use liq2any_chain::polygon::{lock_id, Lock, Rpc as PolygonRpc, State as LockState};
use liq2any_chain::swap::{answer_claim, taker_fee_address, EarlyRefund, TakerClaim};
use liq2any_maker::rfq::{load_keys, PairConfig, RfqConfig};
use liq2any_maker::{accept, Config, Details, PolygonSources};
use liq2any_maker_core::engine::{Action, Config as EngineConfig, Direction, Outcome, State, Terms};
use liq2any_maker_core::policy::Limits;
use liq2any_maker_core::store::Store;
use liq2any_maker_core::LockPoint;
use liq2any_rfq::messages::{self as msg, Accept, AmountSide, Body, Chain, Envelope, MakerTerms, Request, Units, Wei};
use liq2any_rfq::mock_relay::MockRelay;
use liq2any_rfq::relay::{Relay, RelayEvent};
use liq2any_rfq::rules::TakerRules;
use liq2any_rfq::taker::{find_offers, negotiate, send_body, wait_from, Negotiated, TakerSide};
use liq2any_rfq::wrap;
use liq2any_signer::{unhex, Call, Client, Config as SignerConfig, LiquidSource, PolygonConfig, Reply, SwapTerms};
use liq2any_swapkey::{nonce_bytes, nonce_from, point_of, polygon_address_of, polygon_value, share_bytes, share_from};
use nostr::prelude::{Keys, Kind};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::{Keypair as LiquidKey, SecretKey};
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, Script, Sequence, Transaction as LiquidTx};

/// USDT0 base units per swap, and the Liquid side, which has two more decimals.
const USDT0: u64 = 10;
const LIQ_AMOUNT: u64 = USDT0 * 100;
/// POL our L→P lock forwards, in wei.
const GAS_DROP: u128 = 1_000_000_000_000_000;
const NETWORK: &str = "regtest";
const QUOTE_SECS: u64 = 4;
const FLOOD_LIMIT: usize = 12;
/// What the RFQ taker asks to send, at the 1% spread below.
const RFQ_SEND: u64 = 100;
const RFQ_RECEIVE: u64 = 99;
const RFQ_MAX: u64 = 120;
/// Liquid blocks from acceptance to the lock height. L→P must leave the engine's gate of 390 when the maker locks.
const L2P_LOCK_IN: u32 = 480;
const P2L_LOCK_IN: u32 = 80;
/// How long the taker's own P→L lock runs: past the maker's Liquid refund, and the gate's 120 s.
const TAKER_LOCK_SECS: u64 = 600;
/// The test engine's L→P lock length, and P→L gate.
const L2P_SECS: u64 = 90;
const P2L_GATE_SECS: u64 = 120;
type Res<T> = Result<T, String>;

/// Anvil's default accounts: #0 deploys and blocks (Tether's role on the stand-in), #1 is the taker, #2 the signer's key.
const DEPLOYER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TAKER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const TAKER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const SIGNER_KEY: &str = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const ZERO: &str = "0x0000000000000000000000000000000000000000";

fn check(condition: bool, what: &str) -> Res<()> {
    if condition {
        println!("    ok: {what}");
        Ok(())
    } else {
        Err(format!("FAILED: {what}"))
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

struct Node {
    child: Option<Child>,
    daemon: String,
    dir: PathBuf,
    rpc: Rpc,
    wallet_rpc: Rpc,
    port: u16,
}

impl Node {
    fn start(root: &Path) -> Self {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 30_000 + (std::process::id() % 10_000) as u16;
        let dir = root.join("node");
        std::fs::create_dir_all(&dir).unwrap();
        let base: Vec<String> = ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}")]
            .map(String::from)
            .to_vec();
        let mut with_wallet = base.clone();
        with_wallet.push("-rpcwallet=w".into());
        let rpc = Rpc { cli: cli.clone(), args: base };
        let wallet_rpc = Rpc { cli, args: with_wallet };
        let mut node = Node { child: None, daemon, dir, rpc, wallet_rpc, port };
        node.spawn();
        node.rpc.call(&["createwallet", "w"]).unwrap();
        node.rpc.call(&["rescanblockchain"]).unwrap();
        node
    }

    fn spawn(&mut self) {
        let child = Command::new(&self.daemon)
            .arg(format!("-datadir={}", self.dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .arg("-acceptdiscountct=1")
            .args([
                "-persistmempool=0",
                "-rpcuser=t",
                "-rpcpassword=t",
                &format!("-rpcport={}", self.port),
                "-printtoconsole=0",
            ])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd");
        self.child = Some(child);
        let start = Instant::now();
        while self.rpc.call(&["getblockcount"]).is_err() {
            // Say why, and leave nothing running: a stray node kept a failed run waiting until its job timed out.
            let exited = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten());
            if exited.is_some() || start.elapsed() > Duration::from_secs(90) {
                if let Some(mut c) = self.child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let log = std::fs::read_to_string(self.dir.join("liquidregtest/debug.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(25).collect();
                panic!(
                    "elementsd did not start ({exited:?}), port {}:\n{}",
                    self.port,
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Ask the node to stop, and kill it if it hasn't within a minute.
    fn stop(&mut self) {
        let _ = self.rpc.call(&["stop"]);
        if let Some(mut c) = self.child.take() {
            let start = Instant::now();
            while matches!(c.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(60) {
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn mine(&self, blocks: u32) {
        mine_with(&self.wallet_rpc, blocks);
    }

    fn fund(&self, address: &str, sats: u64, label: &str) {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        self.wallet_rpc
            .call(&["sendtoaddress", address, &amount, "", "", "false", "false", "1", "UNSET", "false", label])
            .unwrap();
    }

    /// As the federation would: mark the block invalid, and forget its transactions.
    fn roll_back_to_before(&mut self, height: u32) {
        let hash = self.rpc.call(&["getblockhash", &height.to_string()]).unwrap();
        self.rpc.call(&["invalidateblock", &hash]).unwrap();
        self.stop();
        self.spawn();
        self.rpc.call(&["loadwallet", "w"]).unwrap();
    }
}

fn mine_with(rpc: &Rpc, blocks: u32) {
    if let Ok(address) = rpc.call(&["getnewaddress"]) {
        let _ = rpc.call(&["generatetoaddress", &blocks.to_string(), &address]);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mines Liquid steadily, like a real chain, until told to halt.
struct Miner {
    on: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Miner {
    fn start(rpc: Rpc) -> Self {
        let on = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (o, s) = (on.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                if o.load(Ordering::SeqCst) {
                    mine_with(&rpc, 1);
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        Self { on, stop, handle: Some(handle) }
    }

    fn halt(&self) {
        self.on.store(false, Ordering::SeqCst);
    }

    fn resume(&self) {
        self.on.store(true, Ordering::SeqCst);
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Proc {
    child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cast(args: &[&str]) -> Res<String> {
    let out = Command::new("cast").args(args).output().map_err(err)?;
    if !out.status.success() {
        return Err(format!("cast {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A local EVM chain with the escrow and stand-in token deployed, mining a block a second, with 2,100 blocks of history
/// one second apart so the daemon can measure the block time at once.
struct Evm {
    child: Child,
    url: String,
    rpc: PolygonRpc,
    escrow: EvmAddress,
    token: EvmAddress,
}

impl Evm {
    fn start() -> Self {
        let port = 45_000 + (std::process::id() % 15_000) as u16;
        let url = format!("http://127.0.0.1:{port}");
        // Finalized trails the head by two slots of an epoch of one, as Polygon's milestones trail it by a few blocks.
        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent", "--block-time", "1", "--slots-in-an-epoch", "1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("start anvil");
        let rpc = PolygonRpc::new(&url);
        let start = Instant::now();
        while rpc.chain_id().is_err() {
            assert!(start.elapsed() < Duration::from_secs(30), "anvil did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        cast(&["rpc", "anvil_mine", "0x834", "0x1", "--rpc-url", &url]).unwrap();
        let contracts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts/polygon");
        let deploy = |args: &[&str]| -> EvmAddress {
            let mut all = vec!["create", "--rpc-url", &url, "--private-key", DEPLOYER_KEY, "--broadcast", "--json"];
            all.extend_from_slice(args);
            let out = Command::new("forge").args(&all).current_dir(contracts).output().expect("forge");
            assert!(out.status.success(), "forge create: {}", String::from_utf8_lossy(&out.stderr));
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            EvmAddress::parse(v["deployedTo"].as_str().unwrap()).unwrap()
        };
        let token = deploy(&["src/testnet/StandInUSDT0.sol:StandInUSDT0"]);
        let escrow = deploy(&["src/Escrow.sol:Escrow", "--constructor-args", &token.to_string(), "1209600", "604800"]);
        Evm { child, url, rpc, escrow, token }
    }

    fn send(&self, key: &str, to: &EvmAddress, sig: &str, args: &[&str]) -> Res<()> {
        let to = to.to_string();
        let mut all = vec!["send", "--rpc-url", &self.url, "--private-key", key, &to, sig];
        all.extend_from_slice(args);
        cast(&all).map(|_| ())
    }

    fn lock(&self, id: &[u8; 32]) -> Res<Lock> {
        self.rpc.lock_at(&self.escrow, id, "latest")
    }

    /// The lock, once it exists.
    fn lock_made(&self, id: &[u8; 32]) -> Res<Option<Lock>> {
        Ok(Some(self.lock(id)?).filter(|l| l.state != LockState::None))
    }

    fn now(&self) -> Res<(u64, u64)> {
        let b = self.rpc.block("latest")?;
        Ok((b.time, b.number))
    }

    fn usdt0(&self, who: &EvmAddress) -> Res<u128> {
        self.rpc.token_balance(&self.token, who, "latest")
    }

    fn pol(&self, who: &EvmAddress) -> Res<u128> {
        self.rpc.balance(who, "latest")
    }

    fn block(&self, who: &EvmAddress, on: bool) -> Res<()> {
        let sig = if on { "addToBlockedList(address)" } else { "removeFromBlockedList(address)" };
        self.send(DEPLOYER_KEY, &self.token, sig, &[&who.to_string()])
    }

    /// Stop making blocks, as in a Polygon halt, or start again.
    fn halt(&self, on: bool) -> Res<()> {
        cast(&["rpc", "evm_setIntervalMining", if on { "0" } else { "1" }, "--rpc-url", &self.url]).map(|_| ())
    }

    /// Move both clocks on by `secs`, one block a second, so the measured block time stays the same.
    fn warp(&self, secs: u64) -> Res<()> {
        cast(&["rpc", "anvil_mine", &format!("0x{secs:x}"), "0x1", "--rpc-url", &self.url]).map(|_| ())
    }
}

impl Drop for Evm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn taker() -> EvmAddress {
    EvmAddress::parse(TAKER).unwrap()
}

fn lock_address(point: &LockPoint) -> EvmAddress {
    EvmAddress(polygon_address_of(&point.0).unwrap())
}

fn word(bytes: &[u8; 32]) -> String {
    format!("0x{}", msg::hex(bytes))
}

/// A new address nobody holds, for a gas drop to arrive at.
fn fresh_address() -> Res<EvmAddress> {
    let mut a = [0u8; 20];
    a.copy_from_slice(&random32()?[..20]);
    Ok(EvmAddress(a))
}

struct World {
    node: Node,
    miner: Miner,
    chain: Esplora,
    evm: Evm,
    genesis: BlockHash,
    taker_wallet: Wallet,
    maker: EvmAddress,
    bot: Bot,
    client: Client,
    signer_socket: PathBuf,
    store: Store,
    daemon: Option<Proc>,
    daemon_config: PathBuf,
    pages: PathBuf,
    _signer: Proc,
    fake_asset: AssetId,
    relay: MockRelay,
    relay_url: String,
    maker_nostr: nostr::prelude::PublicKey,
    /// The operator, who gets pages as Nostr private messages, on the mock relay.
    operator: nostr::prelude::Keys,
}

impl World {
    fn start_daemon(&mut self) {
        // Kept next to the config, so a failure can be explained afterwards.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("daemon.log"))
            .expect("open the daemon's log");
        let child = Command::new(env!("CARGO_BIN_EXE_liq2any-maker"))
            .arg(&self.daemon_config)
            .stdout(log)
            .spawn()
            .expect("start daemon");
        self.daemon = Some(Proc { child });
    }

    /// As a crash: no warning, no cleanup.
    fn kill_daemon(&mut self) {
        self.daemon = None;
        println!("    (killed the daemon with no warning)");
    }

    fn state(&self, hash: &[u8; 32]) -> Res<State> {
        Ok(self.store.state(hash).map_err(err)?.ok_or("unknown swap")?.1)
    }

    fn wait_state(&self, hash: &[u8; 32], want: &str, limit: u64, pred: impl Fn(&State) -> bool) -> Res<State> {
        let start = Instant::now();
        loop {
            let state = self.state(hash)?;
            if pred(&state) {
                println!("    ok: reached {want} after {}s", start.elapsed().as_secs());
                return Ok(state);
            }
            if start.elapsed() > Duration::from_secs(limit) {
                return Err(format!("FAILED: never reached {want}, stuck at {state:?}"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_for<T>(&self, what: &str, limit: u64, mut f: impl FnMut() -> Res<Option<T>>) -> Res<T> {
        let start = Instant::now();
        loop {
            if let Ok(Some(v)) = f() {
                println!("    ok: {what} after {}s", start.elapsed().as_secs());
                return Ok(v);
            }
            if start.elapsed() > Duration::from_secs(limit) {
                return Err(format!("FAILED: {what} never happened"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// The maker's Liquid USDt, from its signer: with fresh addresses, only the signer knows every address it holds coins at.
    fn maker_holdings(&self) -> Res<u64> {
        match Client::connect(&self.signer_socket)?.call(&Call::Balances)? {
            Reply::Balances { liquid_usdt, .. } => Ok(liquid_usdt),
            other => Err(format!("no balances: {other:?}")),
        }
    }

    /// The maker's USDT0. A lock's funds sit in that lock's own vault, so they leave this balance while locked.
    fn maker_usdt0(&self) -> Res<u128> {
        self.evm.usdt0(&self.maker)
    }

    fn taker_usdt0(&self) -> Res<u128> {
        self.evm.usdt0(&taker())
    }

    fn maker_key(&mut self, id: [u8; 32]) -> LockPoint {
        let Reply::SwapKey { key } = self.client.call(&Call::SwapKey { swap_id: id }).unwrap() else { panic!() };
        key
    }

    /// The taker's P→L lock, to the maker, on both clocks `secs` from now, as a long leg.
    fn taker_lock(&self, point: &LockPoint, amount: u64, secs: u64) -> Res<[u8; 32]> {
        self.taker_lock_to(point, &self.maker, amount, secs)
    }

    fn taker_lock_to(&self, point: &LockPoint, recipient: &EvmAddress, amount: u64, secs: u64) -> Res<[u8; 32]> {
        let (time, block) = self.evm.now()?;
        let address = lock_address(point);
        let args = [
            address.to_string(),
            recipient.to_string(),
            amount.to_string(),
            (time + secs).to_string(),
            (block + secs).to_string(),
            "true".into(),
            ZERO.into(),
        ];
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.evm.send(TAKER_KEY, &self.evm.escrow, "lock(address,address,uint128,uint64,uint64,bool,address)", &args)?;
        Ok(lock_id(&taker(), &address))
    }

    /// The taker's claim of the maker's L→P lock. It reveals the secret's Polygon form, which is all the maker needs.
    fn taker_claim_polygon(&self, id: &[u8; 32], secret: &[u8; 32]) -> Res<()> {
        let s = polygon_value(secret).map_err(err)?;
        self.evm.send(TAKER_KEY, &self.evm.escrow, "claim(bytes32,bytes32)", &[&word(id), &word(&s)])
    }

    fn taker_refund_polygon(&self, id: &[u8; 32]) -> Res<()> {
        self.evm.send(TAKER_KEY, &self.evm.escrow, "refund(bytes32)", &[&word(id)])
    }

    /// The maker's L→P lock for this point.
    fn maker_lock_id(&self, point: &LockPoint) -> [u8; 32] {
        lock_id(&self.maker, &lock_address(point))
    }
}

/// A taker's wallet at work: it watches for claim offers on its L→P swaps, checks each one, and signs it.
struct Bot {
    keys: Keys,
    swaps: Arc<Mutex<HashMap<String, BotSwap>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone)]
struct BotSwap {
    terms: SwapTerms,
    key: LiquidKey,
    maker_key: LockPoint,
}

impl Bot {
    fn start(url: &str, maker: nostr::prelude::PublicKey, chain: Esplora, genesis: BlockHash) -> Self {
        let keys = Keys::generate();
        let swaps: Arc<Mutex<HashMap<String, BotSwap>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (url, me, map, halt) = (url.to_string(), keys.clone(), swaps.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut relay = Relay::connect(&url, Duration::from_millis(200)).expect("bot relay");
            relay
                .subscribe("bot", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)
                .expect("bot subscribe");
            let mut answered = HashSet::new();
            while !halt.load(Ordering::SeqCst) {
                let Ok(Some(RelayEvent::Event(e))) = relay.next_event() else { continue };
                let Ok(got) = wrap::unwrap(&me, &e) else { continue };
                if got.sender != maker {
                    continue;
                }
                let Body::ClaimOffer(offer) = got.envelope.body else { continue };
                // The maker repeats its offer until the lock appears. Answering once per nonce is enough.
                if !answered.insert(offer.nonce.clone()) {
                    continue;
                }
                let Some(swap) = map.lock().unwrap().get(&offer.lock_point).cloned() else { continue };
                match answer_offer(&chain, &swap, &offer, genesis) {
                    Ok(answer) => {
                        let sent =
                            send_body(&mut relay, &me, &maker, &got.envelope.id, NETWORK, Body::ClaimAnswer(answer));
                        if let Err(e) = sent {
                            eprintln!("bot: {e}");
                        }
                    }
                    Err(e) => eprintln!("bot: refusing an offer: {e}"),
                }
            }
        });
        Self { keys, swaps, stop, thread: Some(thread) }
    }

    fn watch(&self, swap: BotSwap) {
        self.swaps.lock().unwrap().insert(swap.terms.lock_point.to_hex(), swap);
    }
}

impl Drop for Bot {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Check a claim offer and answer it. The coin comes from the taker's own reading of the chain.
fn answer_offer(chain: &Esplora, s: &BotSwap, offer: &msg::ClaimOffer, genesis: BlockHash) -> Res<msg::ClaimAnswer> {
    let address = s.terms.address(&s.maker_key)?;
    let coins = chain.secret_coins(&address, &s.terms.blinding()?)?;
    let coin = exact_secret_coin(&coins, usdt(), s.terms.liquid_amount, chain.tip()?, 0).ok_or("no coin of ours")?;
    let tx: LiquidTx = deserialize(&unhex(&offer.tx)?).map_err(err)?;
    let nonce = nonce_from(&unhex(&offer.nonce)?).map_err(err)?;
    let output = s.terms.output(&s.maker_key)?;
    let (ours, share) =
        answer_claim(chain, &s.key, &output, &coin.coin.outpoint, &s.terms.lock_point.0, &nonce, &tx, genesis)?;
    Ok(msg::ClaimAnswer {
        lock_point: offer.lock_point.clone(),
        nonce: msg::hex(&nonce_bytes(&ours)),
        share: msg::hex(&share_bytes(&share)),
    })
}

/// One simulated taker's swap, in either direction.
struct Swap {
    secret: [u8; 32],
    id: [u8; 32],
    point: LockPoint,
    key: LiquidKey,
    terms: SwapTerms,
    maker_key: LockPoint,
    negotiation: String,
    to: Script,
}

/// Terms as the taker chooses them. L→P names somewhere for a gas drop when `gas_drop` isn't zero.
fn fresh_swap_terms(
    direction: Direction,
    lock_height: u32,
    liquid: u64,
    polygon: u64,
    gas_drop: u128,
) -> Res<(LiquidKey, SwapTerms, [u8; 32])> {
    let secret = random32()?;
    let point = LockPoint(point_of(&secret).map_err(err)?);
    let key = keypair_from(&random32()?)?;
    let terms = SwapTerms {
        direction,
        lock_point: point,
        taker_key: LockPoint(key.public_key().serialize()),
        blinding_key: random32()?,
        lock_height,
        liquid_amount: liquid,
        polygon_amount: polygon,
        taker_polygon: TAKER.into(),
        gas_to: if gas_drop > 0 { Some(fresh_address()?.checksummed()) } else { None },
        gas_drop,
    };
    Ok((key, terms, secret))
}

/// A swap registered straight in the maker's store, as an accepted quote would be.
fn new_swap(w: &mut World, direction: Direction, lock_in: u32, gas_drop: u128) -> Res<Swap> {
    let tip = w.chain.tip()?;
    let (key, terms, secret) = fresh_swap_terms(direction, tip + lock_in, LIQ_AMOUNT, USDT0, gas_drop)?;
    let point = terms.lock_point;
    let id = point.id();
    let maker_key = w.maker_key(id);
    let negotiation = msg::hex(&random32()?);
    let details =
        Details { terms: terms.clone(), taker_nostr: w.bot.keys.public_key().to_hex(), negotiation: negotiation.clone() };
    let engine_terms = Terms {
        direction,
        lock_point: point,
        liquid_amount: LIQ_AMOUNT,
        polygon_amount: USDT0,
        h_accept: tip,
        lock_height: tip + lock_in,
        gas_drop,
    };
    check(accept(&w.store, &engine_terms, &details)?, "the maker accepted the swap")?;
    if direction == Direction::L2P {
        w.bot.watch(BotSwap { terms: terms.clone(), key, maker_key });
    }
    Ok(Swap { secret, id, point, key, terms, maker_key, negotiation, to: w.taker_wallet.address().script_pubkey() })
}

fn new_l2p(w: &mut World) -> Res<Swap> {
    new_swap(w, Direction::L2P, L2P_LOCK_IN, 0)
}

fn new_p2l(w: &mut World, lock_in: u32) -> Res<Swap> {
    new_swap(w, Direction::P2L, lock_in, 0)
}

fn swap_address(s: &Swap) -> Address {
    s.terms.address(&s.maker_key).unwrap()
}

/// Pay from the taker's wallet. A second payment in one block would reuse the first's coins, so that one waits its turn.
fn taker_pays_to(w: &World, address: &Address, asset: AssetId, amount: u64) -> Res<()> {
    let payment = || Payment { asset, amount, address: address.clone() };
    w.wait_for("the taker's payment to be accepted", 60, || {
        let tx = build_confidential_pay(&w.chain, &w.taker_wallet, vec![payment()], w.genesis)?;
        match w.chain.broadcast(&tx) {
            Ok(_) => Ok(Some(())),
            Err(e) if e.contains("conflict") || e.contains("missingorspent") => Ok(None),
            Err(e) => Err(e),
        }
    })
}

fn taker_pays(w: &World, s: &Swap) -> Res<()> {
    taker_pays_to(w, &swap_address(s), usdt(), LIQ_AMOUNT)
}

fn swap_coin(w: &World, s: &Swap, confs: u32) -> Res<Option<SecretCoin>> {
    let coins = w.chain.secret_coins(&swap_address(s), &s.terms.blinding()?)?;
    Ok(exact_secret_coin(&coins, usdt(), LIQ_AMOUNT, w.chain.tip()?, confs))
}

fn fee_coin(w: &World, s: &Swap) -> Res<SecretCoin> {
    let address = s.terms.taker_fee_address()?;
    w.chain.secret_coins(&address, &s.terms.blinding()?)?.into_iter().next().ok_or_else(|| "no fee coin from the maker".into())
}

/// The whole P→L claim on Liquid, over a relay, from an unsigned transaction to one the network accepts.
#[allow(clippy::too_many_arguments)]
fn session_claim(
    w: &World,
    relay: &mut Relay,
    me: &Keys,
    id: &str,
    terms: &SwapTerms,
    key: &LiquidKey,
    maker_key: &LockPoint,
    secret: &[u8; 32],
    fee: &SecretCoin,
    coin: &SecretCoin,
    to: &Address,
) -> Res<LiquidTx> {
    let output = terms.output(maker_key)?;
    let point = terms.lock_point;
    let (tx, _) = build_swap_spend(coin, fee, to, 0, Sequence::MAX)?;
    let claim = TakerClaim::start(&w.chain, key, &output, tx, w.genesis)?;
    let maker = w.maker_nostr;
    let open = Body::SessionOpen(msg::SessionOpen {
        lock_point: point.to_hex(),
        tx: serialize_hex(&claim.tx),
        nonce: msg::hex(&nonce_bytes(&claim.nonce())),
    });
    send_body(relay, me, &maker, id, NETWORK, open)?;
    let got = wait_from(relay, me, &maker, id, NETWORK, Duration::from_secs(30))?;
    let Body::SessionNonce(answer) = got.envelope.body else {
        return Err(format!("expected a nonce, got {:?}", got.envelope.body));
    };
    let maker_nonce = nonce_from(&unhex(&answer.nonce)?).map_err(err)?;
    let (share, signed) = claim.sign(key, &output, &point.0, &maker_nonce)?;
    let body = Body::TakerShare(msg::SessionShare { lock_point: point.to_hex(), share: msg::hex(&share_bytes(&share)) });
    send_body(relay, me, &maker, id, NETWORK, body)?;
    let got = wait_from(relay, me, &maker, id, NETWORK, Duration::from_secs(30))?;
    let Body::MakerShare(answer) = got.envelope.body else {
        return Err(format!("expected a share, got {:?}", got.envelope.body));
    };
    let maker_share = share_from(&unhex(&answer.share)?).map_err(err)?;
    signed.finish(key, &output, &maker_key.0, maker_share, &point.0, secret, w.genesis)
}

/// A genuine request to open a session on a coin that really is locked, so the maker would answer it if the sender and swap
/// were accepted. Returns whether a nonce came back, on this key and this negotiation id.
#[allow(clippy::too_many_arguments)]
fn session_answered(
    w: &World,
    me: &Keys,
    id: &str,
    terms: &SwapTerms,
    key: &LiquidKey,
    maker_key: &LockPoint,
    fee: &SecretCoin,
    coin: &SecretCoin,
    wait: Duration,
) -> Res<bool> {
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    relay.subscribe("probe", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 60)?;
    let (tx, _) = build_swap_spend(coin, fee, &w.taker_wallet.address(), 0, Sequence::MAX)?;
    let claim = TakerClaim::start(&w.chain, key, &terms.output(maker_key)?, tx, w.genesis)?;
    let open = Body::SessionOpen(msg::SessionOpen {
        lock_point: terms.lock_point.to_hex(),
        tx: serialize_hex(&claim.tx),
        nonce: msg::hex(&nonce_bytes(&claim.nonce())),
    });
    send_body(&mut relay, me, &w.maker_nostr, id, NETWORK, open)?;
    match wait_from(&mut relay, me, &w.maker_nostr, id, NETWORK, wait) {
        Ok(got) => Ok(matches!(got.envelope.body, Body::SessionNonce(_))),
        Err(e) if e.contains("in time") => Ok(false),
        Err(e) => Err(e),
    }
}

/// The taker's P→L claim on Liquid, with the swap's own taker key. Publishing it is what reveals the secret to the maker.
fn taker_claims(w: &World, s: &Swap, coin: &SecretCoin) -> Res<LiquidTx> {
    let fee = fee_coin(w, s)?;
    check(fee.secrets.value == FEE && fee.secrets.asset == lbtc(), "the maker sent the taker a fee coin")?;
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let me = w.bot.keys.clone();
    relay.subscribe("claim", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)?;
    let to = w.taker_wallet.address();
    let tx = session_claim(w, &mut relay, &me, &s.negotiation, &s.terms, &s.key, &s.maker_key, &s.secret, &fee, coin, &to)?;
    w.chain.broadcast(&tx)?;
    Ok(tx)
}

/// What a crash leaves behind if it happens right after the adapter signature is stored: done through the signer directly,
/// with the daemon down.
fn store_adaptor_directly(w: &mut World, s: &Swap) -> Res<()> {
    let swap = BotSwap { terms: s.terms.clone(), key: s.key, maker_key: s.maker_key };
    let Reply::ClaimTemplate { tx, nonce } = w.client.call(&Call::PrepareClaim { terms: s.terms.clone() })? else {
        return Err("no claim template".into());
    };
    let offer = msg::ClaimOffer { lock_point: s.point.to_hex(), tx, nonce };
    let answer = answer_offer(&w.chain, &swap, &offer, w.genesis)?;
    let call = Call::StoreAdaptor { terms: s.terms.clone(), taker_nonce: answer.nonce, taker_share: answer.share };
    match w.client.call(&call)? {
        Reply::Held(true) => Ok(()),
        other => Err(format!("not stored: {other:?}")),
    }
}

fn locked(s: &State) -> bool {
    matches!(s, State::LockedPolygon { .. } | State::LockedLiquid)
}

fn done(outcome: Outcome) -> impl Fn(&State) -> bool {
    move |s| *s == State::Done { outcome }
}

/// The maker's L→P lock, once it lands, on the chain's head.
fn maker_lock(w: &World, s: &Swap, limit: u64) -> Res<Lock> {
    let id = w.maker_lock_id(&s.point);
    w.wait_for("the maker's Polygon lock to land", limit, || w.evm.lock_made(&id))
}

/// E1 and E17: a whole L→P swap, with the daemon killed at the two points that matter most. The lock forwards a gas drop.
fn l2p_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, taker_before) = (w.maker_holdings()?, w.taker_usdt0()?);
    let s = new_swap(w, Direction::L2P, L2P_LOCK_IN, GAS_DROP)?;
    let gas_to = EvmAddress::parse(s.terms.gas_to.as_deref().ok_or("no gas_to")?)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "the maker's Polygon lock", 180, locked)?;
    w.kill_daemon();
    let id = w.maker_lock_id(&s.point);
    let lock = w.wait_for("the lock to land anyway", 20, || w.evm.lock_made(&id)).or_else(|_| {
        w.start_daemon();
        w.wait_for("the lock after the restart", 120, || w.evm.lock_made(&id))
    })?;
    if w.daemon.is_none() {
        w.start_daemon();
    }
    check(
        lock.amount == u128::from(USDT0) && lock.recipient == taker() && !lock.long_leg && lock.lock_address == lock_address(&s.point),
        "the lock has the agreed terms, as a short leg",
    )?;
    check(w.evm.pol(&gas_to)? == GAS_DROP, "and the gas drop arrived at the taker's gas_to")?;

    w.taker_claim_polygon(&id, &s.secret)?;
    check(w.taker_usdt0()? == taker_before + u128::from(USDT0), "the taker was paid on Polygon")?;
    w.kill_daemon();
    w.start_daemon();
    w.wait_state(&s.id, "completion after the restart", 180, done(Outcome::Completed))?;
    check(w.maker_holdings()? == maker_before + LIQ_AMOUNT, "the maker was paid on Liquid, exactly once")?;
    Ok(())
}

/// E1 and E17: a whole P→L swap, with crashes. The key check is that the swap address is only ever paid once.
fn p2l_with_crashes(w: &mut World) -> Res<()> {
    let (maker_before, usdt0_before) = (w.maker_holdings()?, w.maker_usdt0()?);
    let s = new_p2l(w, P2L_LOCK_IN)?;
    let id = w.taker_lock(&s.point, USDT0, TAKER_LOCK_SECS)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 120, locked)?;
    w.kill_daemon();
    w.start_daemon();
    let coin = w.wait_for("the lock to confirm", 120, || swap_coin(w, &s, 2))?;
    std::thread::sleep(Duration::from_secs(8)); // Long enough for a buggy retry to pay a second time.
    check(w.chain.outputs(&swap_address(&s))?.len() == 1, "the address was paid exactly once, despite the crash")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "the maker paid exactly the agreed amount")?;

    taker_claims(w, &s, &coin)?;
    w.kill_daemon();
    std::thread::sleep(Duration::from_secs(4));
    w.start_daemon();
    w.wait_state(&s.id, "completion after the restart", 180, done(Outcome::Completed))?;
    check(w.evm.lock(&id)?.state == LockState::Claimed, "the maker claimed the taker's Polygon lock")?;
    check(w.maker_usdt0()? == usdt0_before + u128::from(USDT0), "and holds its USDT0")?;
    Ok(())
}

/// E17, the dangerous window: a crash after the state is saved, but before the action is sent. The state is written with
/// the daemon down, which is exactly what such a crash leaves behind.
fn crash_between_commit_and_send(w: &mut World) -> Res<()> {
    // L→P: the daemon "decided" to lock on Polygon, then died before sending anything. The adapter signature was already
    // stored by the signer, because the engine doesn't decide to lock without it.
    let s = new_l2p(w)?;
    w.kill_daemon();
    taker_pays(w, &s)?;
    let coin_seen = w.wait_for("the taker's coin to confirm", 60, || swap_coin(w, &s, 2))?;
    let start = Instant::now();
    while !matches!(w.client.call(&Call::PrepareClaim { terms: s.terms.clone() })?, Reply::ClaimTemplate { .. }) {
        check(start.elapsed() < Duration::from_secs(90), "a fee coin became ready")?;
        if let Reply::Liquid(hex) = w.client.call(&Call::TopUpFeeCoins)? {
            let tx: LiquidTx = deserialize(&unhex(&hex)?).map_err(err)?;
            let _ = w.chain.broadcast(&tx);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    store_adaptor_directly(w, &s)?;
    // As the engine would decide them: from the finalized block, the time clock and the block clock with its slack.
    let f = w.evm.rpc.block("finalized")?;
    let (deadline_time, deadline_block) = (f.time + L2P_SECS, f.number + L2P_SECS + L2P_SECS / 10);
    let action = Action::LockPolygon { deadline_time, deadline_block, gas_drop: 0 };
    w.store.commit_step(&s.id, &State::LockedPolygon { deadline_time, deadline_block }, &[action]).map_err(err)?;
    let id = w.maker_lock_id(&s.point);
    check(w.evm.lock_made(&id)?.is_none(), "nothing was sent before the crash")?;
    w.start_daemon();
    let lock = w.wait_for("the lock to be sent after the restart", 60, || w.evm.lock_made(&id))?;
    check(
        lock.deadline_time == deadline_time && lock.deadline_block == deadline_block,
        "with the deadlines decided before the crash",
    )?;
    w.taker_claim_polygon(&id, &s.secret)?;
    w.wait_state(&s.id, "completion", 120, done(Outcome::Completed))?;
    check(w.chain.spender(&coin_seen.coin.outpoint)?.is_some(), "the maker collected its coin")?;

    // P→L: the same, on the side that could pay twice.
    let s = new_p2l(w, P2L_LOCK_IN)?;
    let maker_before = w.maker_holdings()?;
    w.kill_daemon();
    w.taker_lock(&s.point, USDT0, TAKER_LOCK_SECS)?;
    w.store.commit_step(&s.id, &State::LockedLiquid, &[Action::LockLiquid]).map_err(err)?;
    check(swap_coin(w, &s, 0)?.is_none(), "nothing was paid before the crash")?;
    w.start_daemon();
    let coin = w.wait_for("the lock to be paid after the restart", 90, || swap_coin(w, &s, 2))?;
    std::thread::sleep(Duration::from_secs(12)); // Longer than the retry interval, so a second payment would show.
    check(w.chain.outputs(&swap_address(&s))?.len() == 1, "the address was paid exactly once")?;
    check(w.maker_holdings()? == maker_before - LIQ_AMOUNT, "for exactly the agreed amount")?;
    taker_claims(w, &s, &coin)?;
    w.wait_state(&s.id, "completion", 120, done(Outcome::Completed))?;
    Ok(())
}

/// E3: the taker pays and signs, then vanishes. The maker takes its USDT0 back at the deadline by itself.
fn l2p_taker_vanishes(w: &mut World) -> Res<()> {
    let before = w.maker_usdt0()?;
    let s = new_l2p(w)?;
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "the maker's Polygon lock", 180, locked)?;
    maker_lock(w, &s, 60)?;
    check(w.maker_usdt0()? == before - u128::from(USDT0), "the funds are locked, in the lock's own vault")?;
    w.wait_state(&s.id, "the maker's automatic refund", 240, done(Outcome::Refunded))?;
    check(w.maker_usdt0()? == before, "the refund came back to the maker")?;
    Ok(())
}

/// A taker who pays but never signs: without an adapter signature the maker risks nothing, so it never locks.
fn l2p_taker_never_signs(w: &mut World) -> Res<()> {
    let before = w.maker_usdt0()?;
    let s = new_l2p(w)?;
    w.bot.swaps.lock().unwrap().remove(&s.point.to_hex());
    taker_pays(w, &s)?;
    w.wait_state(&s.id, "giving up on a taker who never signs", 240, |st| *st == State::Abandoned)?;
    check(w.evm.lock_made(&w.maker_lock_id(&s.point))?.is_none(), "no lock was ever made")?;
    check(w.maker_usdt0()? == before, "no maker funds moved")?;
    Ok(())
}

/// E3: the taker locks, then vanishes. The maker refunds on Liquid at the lock height, and the taker on Polygon later.
fn p2l_taker_vanishes(w: &mut World) -> Res<()> {
    let maker_before = w.maker_holdings()?;
    let taker_before = w.taker_usdt0()?;
    let s = new_p2l(w, 35)?;
    let id = w.taker_lock(&s.point, USDT0, TAKER_LOCK_SECS)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    w.wait_state(&s.id, "the maker's automatic refund", 300, done(Outcome::Refunded))?;
    check(w.maker_holdings()? == maker_before, "the maker has its Liquid funds back")?;
    check(w.taker_refund_polygon(&id).is_err(), "the taker can't refund before its deadline")?;
    w.evm.warp(TAKER_LOCK_SECS)?;
    w.taker_refund_polygon(&id)?;
    check(w.taker_usdt0()? == taker_before, "the taker has its USDT0 back after its deadline")?;
    Ok(())
}

/// E9: bad coins never get a lock. A bad lock never gets one either, and is handed back.
fn bad_locks(w: &mut World) -> Res<()> {
    let before = w.maker_usdt0()?;
    let s = new_l2p(w)?;
    let address = swap_address(&s);
    taker_pays_to(w, &address, usdt(), LIQ_AMOUNT - 100)?;
    // Blinded, so nothing on-chain says what it is. Only reading it does.
    w.node.fund(&address.to_string(), LIQ_AMOUNT, &w.fake_asset.to_string());
    w.wait_state(&s.id, "giving up on a short coin and a fake asset", 240, |st| *st == State::Abandoned)?;
    check(w.evm.lock_made(&w.maker_lock_id(&s.point))?.is_none(), "no lock was ever made")?;
    check(w.maker_usdt0()? == before, "no maker funds moved")?;

    // Short, a short leg, to the wrong recipient, or too near its deadline: each is refused, and the maker's own are handed
    // back at once. The one to a stranger isn't the maker's to release, so the taker takes it back itself.
    let maker_before = w.maker_holdings()?;
    let taker_before = w.taker_usdt0()?;
    let short = new_p2l(w, P2L_LOCK_IN)?;
    let short_id = w.taker_lock(&short.point, USDT0 - 1, TAKER_LOCK_SECS)?;
    let near = new_p2l(w, P2L_LOCK_IN)?;
    let near_id = w.taker_lock(&near.point, USDT0, P2L_GATE_SECS - 30)?;
    let stranger = fresh_address()?;
    let elsewhere = new_p2l(w, P2L_LOCK_IN)?;
    let elsewhere_id = w.taker_lock_to(&elsewhere.point, &stranger, USDT0, TAKER_LOCK_SECS)?;
    let short_leg = new_p2l(w, P2L_LOCK_IN)?;
    let (time, block) = w.evm.now()?;
    let args = [
        lock_address(&short_leg.point).to_string(),
        w.maker.to_string(),
        USDT0.to_string(),
        (time + TAKER_LOCK_SECS).to_string(),
        (block + TAKER_LOCK_SECS).to_string(),
        "false".into(),
        ZERO.into(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    w.evm.send(TAKER_KEY, &w.evm.escrow, "lock(address,address,uint128,uint64,uint64,bool,address)", &args)?;
    let short_leg_id = lock_id(&taker(), &lock_address(&short_leg.point));
    for (s, what) in [(&short, "a short lock"), (&near, "a lock too near its deadline"), (&elsewhere, "a lock to someone else"), (&short_leg, "a short-leg lock")] {
        w.wait_state(&s.id, &format!("giving up on {what}"), 240, |st| *st == State::Abandoned)?;
    }
    check(w.maker_holdings()? == maker_before, "no maker funds moved")?;
    for (id, what) in [(short_id, "short"), (near_id, "near"), (short_leg_id, "short-leg")] {
        w.wait_for(&format!("the {what} lock to be handed back"), 60, || {
            Ok((w.evm.lock(&id)?.state == LockState::Refunded).then_some(()))
        })?;
    }
    check(w.evm.lock(&elsewhere_id)?.state == LockState::Open, "the lock to a stranger is left alone")?;
    check(
        w.taker_usdt0()? == taker_before - u128::from(USDT0),
        "the taker has back all but the lock it sent to a stranger",
    )?;
    Ok(())
}

/// E11: Liquid stalls with P→L open. With no help, the daemon extends the taker's deadline on both clocks, and the swap
/// finishes safely.
fn liquid_halt_extends(w: &mut World) -> Res<()> {
    let s = new_p2l(w, P2L_LOCK_IN)?;
    let id = w.taker_lock(&s.point, USDT0, TAKER_LOCK_SECS)?;
    let before = w.evm.lock(&id)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || swap_coin(w, &s, 2))?;

    w.miner.halt();
    println!("    Liquid halts now");
    let e = w.wait_for("the daemon to extend the deadline by itself", 150, || {
        Ok(Some(w.evm.lock(&id)?).filter(|l| l.extended))
    })?;
    check(
        e.deadline_time > before.deadline_time && e.deadline_block > before.deadline_block,
        "the taker's refund moved later, on both clocks",
    )?;
    w.evm.warp(TAKER_LOCK_SECS + 30)?;
    check(w.taker_refund_polygon(&id).is_err(), "the taker can't refund after the original deadline: the theft is closed")?;

    println!("    Liquid resumes");
    w.miner.resume();
    taker_claims(w, &s, &coin)?;
    w.wait_state(&s.id, "completion after the halt", 180, done(Outcome::Completed))?;
    check(w.evm.lock(&id)?.state == LockState::Claimed, "the maker claimed on Polygon")?;
    Ok(())
}

/// Polygon's finality stalls, as it did for hours in 2025 (docs/assumptions.md). No new lock is made on a stale view, and
/// none is lost: once blocks finalize again, the swap goes ahead.
fn polygon_stall_holds_locks(w: &mut World) -> Res<()> {
    let s = new_l2p(w)?;
    w.evm.halt(true)?;
    println!("    Polygon stops finalizing");
    // Past the engine's 10 s of staleness before the coin can confirm, or the maker rightly locks on a view still fresh.
    std::thread::sleep(Duration::from_secs(12));
    taker_pays(w, &s)?;
    w.wait_for("the taker's coin to confirm", 60, || swap_coin(w, &s, 2))?;
    // Long enough to ask for the adapter signature and decide, with everything else in place.
    std::thread::sleep(Duration::from_secs(10));
    check(w.state(&s.id)? == State::WatchLock, "no lock is decided on a stale Polygon")?;
    let id = w.maker_lock_id(&s.point);
    check(w.evm.lock_made(&id)?.is_none(), "and none is made")?;
    println!("    Polygon resumes");
    w.evm.halt(false)?;
    let lock = maker_lock(w, &s, 90)?;
    let f = w.evm.rpc.block("finalized")?;
    check(lock.deadline_time > f.time, "the lock's deadline is counted from the resumed chain")?;
    w.taker_claim_polygon(&id, &s.secret)?;
    w.wait_state(&s.id, "completion", 120, done(Outcome::Completed))?;
    Ok(())
}

/// Tether can block an address. A blocked taker would be paid by a claim and could never move it, and a blocked maker
/// couldn't move what it claims, so neither swap starts. The taker's lock is handed back, which a blocked maker can still do.
fn blocked_addresses(w: &mut World) -> Res<()> {
    let usdt0_before = w.maker_usdt0()?;
    let liquid_before = w.maker_holdings()?;
    let taker_before = w.taker_usdt0()?;
    w.evm.block(&taker(), true)?;
    let l2p = new_l2p(w)?;
    taker_pays(w, &l2p)?;
    w.evm.block(&taker(), false)?;
    w.evm.block(&w.maker, true)?;
    // Unblocked to lock, and blocked again before the maker reads it: the order a real block could come in.
    let p2l = new_p2l(w, P2L_LOCK_IN)?;
    w.evm.block(&w.maker, false)?;
    let p2l_id = w.taker_lock(&p2l.point, USDT0, TAKER_LOCK_SECS)?;
    w.evm.block(&w.maker, true)?;
    w.evm.block(&taker(), true)?;
    let gave_up = w.wait_state(&l2p.id, "giving up on a blocked taker", 240, |st| *st == State::Abandoned);
    let released = gave_up.and_then(|_| {
        w.wait_state(&p2l.id, "giving up while the maker is blocked", 240, |st| *st == State::Abandoned)?;
        w.wait_for("the taker's lock to be handed back", 60, || {
            Ok((w.evm.lock(&p2l_id)?.state == LockState::Refunded).then_some(()))
        })
    });
    w.evm.block(&w.maker, false)?;
    w.evm.block(&taker(), false)?;
    released?;
    check(w.evm.lock_made(&w.maker_lock_id(&l2p.point))?.is_none(), "no lock was made for the blocked taker")?;
    check(w.maker_usdt0()? == usdt0_before && w.maker_holdings()? == liquid_before, "no maker funds moved")?;
    check(w.taker_usdt0()? == taker_before, "the taker has all its USDT0 back")?;
    Ok(())
}

/// E15: a rollback past the taker's claim. The daemon stops and pages, and doesn't take the coin back.
fn rollback_stops_everything(w: &mut World) -> Res<()> {
    // A second swap, locked first so it survives the rollback, on which a signing session would otherwise go ahead.
    let open = new_p2l(w, P2L_LOCK_IN)?;
    w.taker_lock(&open.point, USDT0, TAKER_LOCK_SECS)?;
    w.wait_state(&open.id, "the second swap's Liquid lock", 60, locked)?;
    let open_coin = w.wait_for("the second lock to confirm", 90, || swap_coin(w, &open, 2))?;
    let open_fee = fee_coin(w, &open)?;

    let s = new_p2l(w, 45)?;
    w.taker_lock(&s.point, USDT0, TAKER_LOCK_SECS)?;
    w.wait_state(&s.id, "the maker's Liquid lock", 60, locked)?;
    let coin = w.wait_for("the lock to confirm", 90, || swap_coin(w, &s, 2))?;
    let claim_tx = taker_claims(w, &s, &coin)?;
    let claim_id = claim_tx.txid();
    w.wait_state(&s.id, "the maker collecting on Polygon", 120, done(Outcome::Completed))?;
    let claim_height = w.wait_for("the taker's claim to confirm", 60, || {
        let rpc = &w.node.rpc;
        let info: serde_json::Value =
            serde_json::from_str(&rpc.call(&["getrawtransaction", &claim_id.to_string(), "1"])?).map_err(err)?;
        let Some(hash) = info["blockhash"].as_str() else { return Ok(None) };
        let header: serde_json::Value = serde_json::from_str(&rpc.call(&["getblockheader", hash])?).map_err(err)?;
        Ok(header["height"].as_u64().map(|h| h as u32))
    })?;

    w.miner.halt();
    let usdt0 = w.maker_usdt0()?;
    println!("    The federation rolls Liquid back past the claim");
    w.kill_daemon();
    let lock_height = s.terms.lock_height;
    w.node.roll_back_to_before(claim_height);
    w.start_daemon();
    w.wait_for("the daemon to raise the rollback alarm", 60, || {
        Ok(std::fs::read_to_string(&w.pages).ok().filter(|t| t.contains("Rollback")))
    })?;
    w.wait_for("a low-balance alarm to reach the operator too", 60, || {
        Ok(w.relay.stored(wrap::GIFT_WRAP_KIND).iter().find_map(|e| {
            wrap::open_private_message(&w.operator, e)
                .ok()
                .filter(|(from, text)| *from == w.maker_nostr && text.contains("enough for fewer than 1000000000 swaps"))
        }))
    })?;
    w.wait_for("the alarm to reach the operator as a private message from the maker", 60, || {
        Ok(w.relay.stored(wrap::GIFT_WRAP_KIND).iter().find_map(|e| {
            wrap::open_private_message(&w.operator, e)
                .ok()
                .filter(|(from, text)| *from == w.maker_nostr && text.contains("Rollback"))
        }))
    })?;
    check(w.store.rollback().map_err(err)?, "the alarm is latched")?;
    check(w.chain.spender(&coin.coin.outpoint)?.is_none(), "the claim is gone, so the coin looks unspent")?;
    let bot = w.bot.keys.clone();
    let ask = |w: &World, wait| {
        session_answered(w, &bot, &open.negotiation, &open.terms, &open.key, &open.maker_key, &open_fee, &open_coin, wait)
    };
    check(!ask(w, SESSION_WAIT)?, "no signing session runs while the alarm is on")?;

    // A coin that really is there, confirmed after the rollback: everything but the alarm says to lock.
    let waiting = new_l2p(w)?;
    taker_pays(w, &waiting)?;
    w.node.mine(2);
    check(swap_coin(w, &waiting, 2)?.is_some(), "a confirmed coin is waiting")?;
    std::thread::sleep(Duration::from_secs(10));
    check(w.state(&waiting.id)? == State::WatchLock, "the alarm stops a lock that would otherwise go ahead")?;
    check(w.evm.lock_made(&w.maker_lock_id(&waiting.point))?.is_none(), "no lock was made")?;
    check(w.maker_usdt0()? == usdt0, "no USDT0 moved")?;

    // An operator clears the alarm, with the real command. It must stay clear, and the waiting swap goes ahead.
    let status = Command::new(env!("CARGO_BIN_EXE_liq2any-maker"))
        .args(["clear-rollback", w.daemon_config.to_str().ok_or("path")?])
        .stdout(Stdio::null())
        .status()
        .map_err(err)?;
    check(status.success(), "the operator cleared the alarm")?;
    w.wait_state(&waiting.id, "the waiting swap going ahead once the alarm is cleared", 120, locked)?;
    check(!w.store.rollback().map_err(err)?, "the alarm stayed clear")?;
    check(ask(w, Duration::from_secs(20))?, "and the same request is answered once it is cleared")?;
    maker_lock(w, &waiting, 60)?;

    // Past the lock height, with the coin unspent and no alarm, an honest maker still leaves it alone.
    while w.chain.tip()? < lock_height {
        w.node.mine(5);
    }
    std::thread::sleep(Duration::from_secs(10));
    check(w.chain.spender(&coin.coin.outpoint)?.is_none(), "the maker didn't refund a coin it already collected for")?;
    w.chain.broadcast(&claim_tx)?;
    w.node.mine(1);
    let spend = w.chain.spender(&coin.coin.outpoint)?.ok_or("not spent")?;
    check(spend.output[0].script_pubkey == s.to, "the taker's rebroadcast claim still collects, however late")?;
    w.miner.resume();
    Ok(())
}

/// The signer is built in the same workspace, so it sits next to the daemon's binary.
fn signer_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_liq2any-maker")).with_file_name("liq2any-signer")
}

fn build_world(root: &Path) -> World {
    let node = Node::start(root);
    node.mine(1);
    let parse = |t: String| -> serde_json::Value { serde_json::from_str(&t).unwrap() };
    let issue = || parse(node.wallet_rpc.call(&["issueasset", "1000", "0", "false"]).unwrap())["asset"].as_str().unwrap().to_string();
    let usdt_id = issue();
    let fake = issue();
    node.mine(1);
    let lbtc_id = parse(node.rpc.call(&["getsidechaininfo"]).unwrap())["pegged_asset"].as_str().unwrap().to_string();
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(&usdt_id).unwrap(),
        lbtc: AssetId::from_str(&lbtc_id).unwrap(),
    });
    let chain = Esplora::Node(node.rpc.clone());
    let genesis = BlockHash::from_str(&chain.genesis().unwrap()).unwrap();

    let evm = Evm::start();
    println!("anvil at {}: escrow {}, token {}", evm.url, evm.escrow, evm.token);
    let signer_address = EvmAddress::parse("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC").unwrap();
    evm.send(SIGNER_KEY, &evm.token, "faucet(uint256)", &["1000000000"]).unwrap();
    evm.send(TAKER_KEY, &evm.token, "faucet(uint256)", &["1000000000"]).unwrap();
    evm.send(TAKER_KEY, &evm.token, "approve(address,uint256)", &[&evm.escrow.to_string(), &u128::MAX.to_string()]).unwrap();

    // The signer's Polygon key is anvil's account #2, so it holds test ETH from the start.
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("polygon.key"), from_hex(SIGNER_KEY).unwrap()).unwrap();
    // Short and outside the build directory: a Unix socket path is limited to about 100 bytes.
    let private = std::env::temp_dir().join(format!("l2a-{}", std::process::id()));
    std::fs::create_dir_all(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = private.join("signer.sock");

    // Short timeouts, so failures play out in minutes. The rules are unchanged.
    let engine = EngineConfig {
        l2p_deadline_secs: L2P_SECS,
        p2l_gate_secs: P2L_GATE_SECS,
        polygon_stale_secs: 10,
        extend_after_stall_secs: 20,
        extend_within_secs: 2_000,
        extend_by_secs: 3_600,
        // The escrow deployed above allows 604,800 blocks of extension.
        max_extension_blocks: 604_800,
        ..EngineConfig::default()
    };
    let liquid = LiquidSource::Node { cli: node.rpc.cli.clone(), args: node.rpc.args.clone() };
    let assets = Some((usdt_id.clone(), lbtc_id.clone()));
    let signer_cfg = SignerConfig {
        network: "testnet".into(),
        socket: socket.clone(),
        ledger: root.join("signer.db"),
        keys_dir: keys.clone(),
        liquid: liquid.clone(),
        polygon: PolygonConfig {
            rpc: evm.url.clone(),
            escrow: Some(evm.escrow.to_string()),
            token: Some(evm.token.to_string()),
            fees: Default::default(),
        },
        limits: Limits::from(&engine),
        regtest_assets: assets.clone(),
        fee_coin_sats: FEE,
        // Every scenario runs with fresh addresses, as a careful operator with its own node would.
        fresh_addresses: true,
        withdraw_to: Default::default(),
    };
    let signer_path = root.join("signer.json");
    std::fs::write(&signer_path, serde_json::to_string(&signer_cfg).unwrap()).unwrap();
    let signer = Proc { child: Command::new(signer_binary()).arg(&signer_path).spawn().expect("start signer") };
    let start = Instant::now();
    while Client::connect(&socket).is_err() {
        assert!(start.elapsed() < Duration::from_secs(60), "signer did not start");
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut client = Client::connect(&socket).unwrap();
    let Reply::Identity { liquid_address, polygon, .. } = client.call(&Call::Identity).unwrap() else { panic!() };
    let maker = EvmAddress::parse(&polygon).unwrap();
    assert_eq!(maker, signer_address, "the signer runs on anvil's account #2, from the key file");

    let pages = root.join("pages.log");
    let db = root.join("maker.db");
    let relay = MockRelay::start();
    // `RFQ_RELAY` runs the swaps through a real relay. The attack scenario always stays on the mock.
    let public_relay = std::env::var("RFQ_RELAY").ok();
    let relay_url = public_relay.clone().unwrap_or_else(|| relay.url());
    let operator = nostr::prelude::Keys::generate();
    let pair = |from, to, prices, lock_blocks, gas_drop| PairConfig { from, to, min: 1, max: RFQ_MAX, prices, lock_blocks, gas_drop: Wei(gas_drop) };
    let cfg = Config {
        network: "testnet".into(),
        // More gas than any test wallet holds, so the alarm always fires, and the check below sees it arrive.
        balance_alarms: liq2any_maker::balances::BalanceAlarms { gas_swaps: 1_000_000_000, ..Default::default() },
        page_to: Some(liq2any_maker::pager::PageTo { nostr: operator.public_key().to_hex(), relays: vec![relay.url()] }),
        db: db.clone(),
        signer_socket: socket.clone(),
        pages: pages.clone(),
        engine,
        liquid,
        second_liquid: None,
        polygon: PolygonSources {
            rpc: evm.url.clone(),
            second_rpc: None,
            escrow: Some(evm.escrow.checksummed()),
            token: Some(evm.token.checksummed()),
        },
        tick_millis: 1_000,
        retry_secs: 8,
        regtest_assets: assets,
        rfq: Some(RfqConfig {
            nostr_key: root.join("nostr.key"),
            relays: vec![relay_url.clone()],
            network: NETWORK.into(),
            pairs: vec![
                // Two steps, so prices are read by size.
                pair(
                    Chain::Liquid,
                    Chain::Polygon,
                    vec![msg::PriceTier { up_to: Units(110), bps: 100 }, msg::PriceTier { up_to: Units(RFQ_MAX), bps: 150 }],
                    L2P_LOCK_IN,
                    GAS_DROP,
                ),
                pair(Chain::Polygon, Chain::Liquid, vec![msg::PriceTier { up_to: Units(RFQ_MAX), bps: 100 }], P2L_LOCK_IN, 0),
            ],
            show_available_up_to: None,
            quote_secs: QUOTE_SECS,
            // Don't keep writing to a public relay. Offers expire soon after the run.
            offer_every_secs: if public_relay.is_some() { 300 } else { 2 },
            offer_ttl_secs: if public_relay.is_some() { 600 } else { 60 },
            max_requests_per_sender: FLOOD_LIMIT,
            max_requests_per_window: 10_000,
            window_secs: 300,
            max_negotiations: 1_000,
        }),
    };
    let maker_nostr = load_keys(&root.join("nostr.key")).unwrap().public_key();
    let daemon_config = root.join("maker.json");
    std::fs::write(&daemon_config, serde_json::to_string(&cfg).unwrap()).unwrap();

    let taker_wallet = Wallet {
        key: keypair_from(&random32().unwrap()).unwrap(),
        blinding: SecretKey::from_slice(&random32().unwrap()).unwrap(),
    };
    for address in [liquid_address.clone(), taker_wallet.address().to_string()] {
        for _ in 0..8 {
            node.fund(&address, 200_000, "bitcoin");
        }
        node.fund(&address, 40 * LIQ_AMOUNT, &usdt_id);
    }
    node.mine(1);
    let miner = Miner::start(node.wallet_rpc.clone());

    let store = Store::open(&db).unwrap();
    let bot = Bot::start(&relay_url, maker_nostr, chain.clone(), genesis);
    let mut world = World {
        node,
        miner,
        chain,
        evm,
        genesis,
        taker_wallet,
        maker,
        bot,
        client,
        signer_socket: socket.clone(),
        store,
        daemon: None,
        daemon_config,
        pages,
        _signer: signer,
        fake_asset: AssetId::from_str(&fake).unwrap(),
        relay,
        relay_url,
        maker_nostr,
        operator,
    };
    world.start_daemon();
    world
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn taker_rules(w: &World) -> TakerRules {
    TakerRules {
        network: NETWORK.into(),
        usdt_asset: usdt().to_string(),
        chain_id: w.evm.rpc.chain_id().unwrap(),
        escrow: w.evm.escrow.checksummed(),
        token: w.evm.token.checksummed(),
        template_version: msg::TEMPLATE_VERSION,
        l2p_min_blocks: 420,
        l2p_max_blocks: 540,
        p2l_min_blocks: 50,
        p2l_max_blocks: 150,
        l2p_min_window_secs: 30,
        l2p_max_window_secs: 120,
        p2l_max_window_secs: 1_000,
        max_spread_bps: 100,
    }
}

fn rfq_request(from: Chain, to: Chain) -> Request {
    Request { from, to, amount_side: AmountSide::Send, amount: Units(RFQ_SEND) }
}

fn l2p_request() -> Request {
    rfq_request(Chain::Liquid, Chain::Polygon)
}

/// What the taker tells the maker about itself. L→P names where the gas drop goes.
fn side_of(chosen: &SwapTerms, gas_to: Option<&EvmAddress>) -> TakerSide {
    TakerSide {
        lock_point: chosen.lock_point.0,
        taker_key: chosen.taker_key.0,
        blinding_key: chosen.blinding_key,
        taker_polygon: TAKER.into(),
        gas_to: gas_to.map(EvmAddress::checksummed),
    }
}

/// The terms the taker will hold the maker to, built from what the taker chose and what it just checked.
fn agreed_terms(s: &SwapTerms, terms: &MakerTerms) -> Res<(SwapTerms, LockPoint)> {
    let maker_key = LockPoint(msg::hex33(&terms.maker_key).ok_or("bad maker key")?);
    let agreed = SwapTerms {
        lock_height: terms.lock_height,
        liquid_amount: terms.liquid_amount.0,
        polygon_amount: terms.polygon_amount.0,
        gas_drop: terms.gas_drop.0,
        ..s.clone()
    };
    Ok((agreed, maker_key))
}

/// E1 over Nostr: L→P, where the taker derives the address itself from the agreed terms, and only then pays it.
fn rfq_l2p_swap(w: &mut World) -> Res<()> {
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let offers = find_offers(&mut relay, NETWORK, Duration::from_secs(5))?;
    let offer = offers.iter().find(|(k, _)| *k == w.maker_nostr).map(|(_, o)| o.clone()).ok_or("no offer from the maker")?;
    println!("    ok: the taker found the maker's offer");
    check(
        offer.chains.polygon.escrow.eq_ignore_ascii_case(&w.evm.escrow.to_string()) && offer.chains.polygon.max_gas_drop == Wei(GAS_DROP),
        "the offer names the escrow and the gas drop",
    )?;

    let (key, chosen, secret) = fresh_swap_terms(Direction::L2P, 0, 0, 0, 0)?;
    let id = chosen.swap_id();
    let gas_to = fresh_address()?;
    let chain = w.chain.clone();
    let maker = w.maker_nostr;
    let Negotiated { id: negotiation, quote, terms } = negotiate(
        &mut relay,
        &me,
        &maker,
        &taker_rules(w),
        l2p_request(),
        side_of(&chosen, Some(&gas_to)),
        move || chain.tip(),
        Duration::from_secs(15),
    )?;
    check(quote.send.0 == RFQ_SEND && quote.receive.0 == RFQ_RECEIVE, "the quote carries the agreed spread")?;
    check(quote.gas_drop == Wei(GAS_DROP) && terms.gas_drop == Wei(GAS_DROP), "and the gas drop")?;
    check(matches!(w.state(&id)?, State::WatchLock | State::LockedPolygon { .. }), "the maker registered the swap")?;
    check(terms.liquid_amount.0 == RFQ_SEND * 100, "the taker pays exactly what was quoted")?;
    let (agreed, maker_key) = agreed_terms(&SwapTerms { gas_to: Some(gas_to.checksummed()), ..chosen.clone() }, &terms)?;
    taker_pays_to(w, &agreed.address(&maker_key)?, usdt(), terms.liquid_amount.0)?;

    // The maker offers its claim once the coin confirms. The maker can't use the signature until we reveal the secret.
    let swap = BotSwap { terms: agreed, key, maker_key };
    let got = wait_from(&mut relay, &me, &maker, &negotiation, NETWORK, Duration::from_secs(120))?;
    let Body::ClaimOffer(offer) = got.envelope.body else {
        return Err(format!("expected a claim offer, got {:?}", got.envelope.body));
    };
    let answer = answer_offer(&w.chain, &swap, &offer, w.genesis)?;
    // With fresh addresses, the maker's claim pays a new address of its own, which its wallet then reads.
    let claim: LiquidTx = deserialize(&unhex(&offer.tx)?).map_err(err)?;
    let Reply::Identity { liquid_address, .. } = w.client.call(&Call::Identity)? else {
        return Err("the signer gave no identity".into());
    };
    let funding = Address::from_str(&liquid_address).map_err(err)?.script_pubkey();
    check(claim.output[0].script_pubkey != funding, "the maker's claim pays a new address, not the one it is funded at")?;
    let before_claim = w.maker_holdings()?;
    send_body(&mut relay, &me, &maker, &negotiation, NETWORK, Body::ClaimAnswer(answer))?;

    let maker_polygon = EvmAddress::parse(&terms.maker_polygon)?;
    check(maker_polygon == w.maker, "the terms name the maker's own address")?;
    let lock_id_ = lock_id(&maker_polygon, &lock_address(&chosen.lock_point));
    let lock = w.wait_for("the maker to lock on Polygon", 120, || w.evm.lock_made(&lock_id_))?;
    check(lock.amount == u128::from(RFQ_RECEIVE) && lock.recipient == taker(), "the lock pays the taker the quoted amount")?;
    let (f_time, _) = w.evm.now()?;
    check(
        lock.deadline_time <= f_time + terms.polygon_window_secs,
        "and runs no longer than the terms said",
    )?;
    check(w.evm.pol(&gas_to)? == GAS_DROP, "the gas drop arrived, so the taker can pay for its claim")?;
    let before = w.taker_usdt0()?;
    w.taker_claim_polygon(&lock_id_, &secret)?;
    check(w.taker_usdt0()? == before + u128::from(RFQ_RECEIVE), "the taker was paid")?;
    w.wait_state(&id, "the maker completing the swap", 120, done(Outcome::Completed))?;
    let amount = terms.liquid_amount.0;
    w.wait_for("the maker's wallet to count the claimed coin at its new address", 60, || {
        Ok((w.maker_holdings()? == before_claim + amount).then_some(()))
    })?;
    Ok(())
}

/// A P→L negotiation up to the point where the taker locks.
struct P2lAgreed {
    relay: Relay,
    me: Keys,
    negotiation: String,
    terms: MakerTerms,
    chosen: SwapTerms,
    key: LiquidKey,
    secret: [u8; 32],
}

fn p2l_agreed(w: &World) -> Res<P2lAgreed> {
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let (key, chosen, secret) = fresh_swap_terms(Direction::P2L, 0, 0, 0, 0)?;
    let chain = w.chain.clone();
    let maker = w.maker_nostr;
    let Negotiated { id: negotiation, terms, .. } = negotiate(
        &mut relay,
        &me,
        &maker,
        &taker_rules(w),
        rfq_request(Chain::Polygon, Chain::Liquid),
        side_of(&chosen, None),
        move || chain.tip(),
        Duration::from_secs(15),
    )?;
    Ok(P2lAgreed { relay, me, negotiation, terms, chosen, key, secret })
}

fn release_ask(a: &P2lAgreed) -> Body {
    Body::ReleaseAsk(msg::ReleaseAsk { lock_point: a.chosen.lock_point.to_hex() })
}

/// The taker's P→L lock to the maker the terms name, leaving the window the terms ask for and a margin.
fn p2l_lock(w: &World, a: &P2lAgreed, secs: u64) -> Res<[u8; 32]> {
    let recipient = EvmAddress::parse(&a.terms.maker_polygon)?;
    w.taker_lock_to(&a.chosen.lock_point, &recipient, a.terms.polygon_amount.0, secs)
}

/// E1 over Nostr: P→L.
fn rfq_p2l_swap(w: &mut World) -> Res<()> {
    let a = p2l_agreed(w)?;
    let ask = release_ask(&a);
    let lock = p2l_lock(w, &a, a.terms.polygon_window_secs + TAKER_LOCK_SECS)?;
    let P2lAgreed { mut relay, me, negotiation, terms, chosen, key, secret } = a;
    let id = chosen.swap_id();
    check(
        terms.liquid_amount.0 == RFQ_RECEIVE * 100 && terms.polygon_amount.0 == RFQ_SEND && terms.gas_drop == Wei(0),
        "the amounts match the quote, with no gas drop",
    )?;

    let (agreed, maker_key) = agreed_terms(&chosen, &terms)?;
    let address = agreed.address(&maker_key)?;
    let blinding = agreed.blinding()?;
    let amount = terms.liquid_amount.0;
    let coin = w.wait_for("the maker to lock on Liquid", 120, || {
        Ok(exact_secret_coin(&w.chain.secret_coins(&address, &blinding)?, usdt(), amount, w.chain.tip()?, 2))
    })?;
    let fee_address = agreed.taker_fee_address()?;
    let fee = w.chain.secret_coins(&fee_address, &blinding)?.into_iter().next().ok_or("no fee coin")?;
    check(fee.secrets.value == FEE && fee.secrets.asset == lbtc(), "the fee coin is what was promised")?;
    let to = w.taker_wallet.address();

    // Having locked on Liquid, the maker never hands the taker's lock back, or the taker could have both sides.
    send_body(&mut relay, &me, &w.maker_nostr, &negotiation, NETWORK, ask)?;
    let got = early_answer(&mut relay, &me, &w.maker_nostr, &negotiation);
    check(got.is_err_and(|e| e.contains("declined")), "having locked on Liquid, the maker won't release")?;
    check(w.evm.lock(&lock)?.state == LockState::Open, "and the taker's lock stays")?;

    // Real requests on a real coin, so only the sender and id checks can refuse them. The real session below is the control.
    let stranger = Keys::generate();
    check(
        !session_answered(w, &stranger, &negotiation, &agreed, &key, &maker_key, &fee, &coin, SESSION_WAIT)?,
        "a stranger gets nothing from a swap's signing session, even with the right id",
    )?;
    let other_id = msg::hex(&random32()?);
    check(
        !session_answered(w, &me, &other_id, &agreed, &key, &maker_key, &fee, &coin, SESSION_WAIT)?,
        "the swap's own taker gets nothing under another negotiation id",
    )?;

    let usdt0_before = w.maker_usdt0()?;
    let claim = session_claim(w, &mut relay, &me, &negotiation, &agreed, &key, &maker_key, &secret, &fee, &coin, &to)?;
    println!("    the claim went through the relay in pieces, and is {} bytes", serialize_hex(&claim).len() / 2);
    w.chain.broadcast(&claim)?;
    w.wait_state(&id, "the maker completing the swap", 120, done(Outcome::Completed))?;
    check(w.maker_usdt0()? == usdt0_before + u128::from(RFQ_SEND), "the maker claimed the taker's USDT0")?;

    // The report, as an operator would run it, after this swap and the ones before it.
    let printed = Command::new(env!("CARGO_BIN_EXE_liq2any-maker")).arg("report").arg(&w.daemon_config).output().map_err(err)?;
    let text = String::from_utf8_lossy(&printed.stdout).to_string();
    println!("{text}");
    check(printed.status.success(), "the report runs while the maker does")?;
    let r = w.store.report().map_err(err)?;
    // Other scenarios' swaps have no spread, and a long run can cross midnight, so any day's line may be this swap's.
    let earned = r.swaps.iter().any(|((day, dir, outcome), l)| day.is_some() && dir == "P2L" && outcome == "Completed" && l.spread > 0);
    check(earned, "it shows this swap, on a day, with its spread")?;
    let (sats, gwei) = r.costs.values().fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1));
    check(sats >= FEE && gwei >= (GAS_DROP / 1_000_000_000) as i64, "and what the maker spent: Liquid fees, and gas drops")?;
    check(text.contains("P2L") && text.contains("Completed"), "and prints it")?;
    let Reply::Balances { liquid_usdt, pol_wei, polygon_usdt, .. } = w.client.call(&Call::Balances)? else {
        return Err("the signer gave no balances".into());
    };
    check(liquid_usdt > 0 && pol_wei > 0 && polygon_usdt > 0, "the signer reads its balances")?;
    check(text.contains(&format!("{:.8} USDt", liquid_usdt as f64 / 1e8)), "and the report ends with them")?;
    Ok(())
}

/// A taker speaking the wire protocol by hand, so it can break the rules.
struct RawTaker {
    keys: Keys,
    relay: Relay,
    maker: nostr::prelude::PublicKey,
    id: String,
}

impl RawTaker {
    fn new(w: &World) -> Res<Self> {
        let keys = Keys::generate();
        let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
        relay.subscribe("raw", &[Kind::from(msg::WRAP_KIND)], Some(&keys.public_key()), now_secs() - 2 * 86_400)?;
        Ok(Self { keys, relay, maker: w.maker_nostr, id: msg::hex(&random32()?) })
    }

    fn send_at(&mut self, id: &str, network: &str, sent_at: u64, body: Body) -> Res<String> {
        let envelope = Envelope { v: msg::VERSION, id: id.to_string(), network: network.into(), sent_at, body };
        let (event, rumor_id) = wrap::wrap(&self.keys, &self.maker, &envelope, now_secs()).map_err(|e| format!("{e:?}"))?;
        self.relay.publish(&event)?;
        Ok(rumor_id)
    }

    fn send(&mut self, body: Body) -> Res<String> {
        let id = self.id.clone();
        self.send_at(&id, NETWORK, now_secs(), body)
    }

    fn answer(&mut self, wait: Duration) -> Res<Option<wrap::Received>> {
        let start = Instant::now();
        while start.elapsed() < wait {
            if let Some(RelayEvent::Event(e)) = self.relay.next_event()? {
                if let Ok(got) = wrap::unwrap(&self.keys, &e) {
                    return Ok(Some(got));
                }
            }
        }
        Ok(None)
    }

    fn quote(&mut self, wait: Duration) -> Res<(msg::Quote, String)> {
        let got = self.answer(wait)?.ok_or("no quote")?;
        match got.envelope.body {
            Body::RfqQuote(q) => Ok((q, got.rumor_id)),
            other => Err(format!("expected a quote, got {other:?}")),
        }
    }
}

fn fresh_id() -> Res<(LockPoint, [u8; 32])> {
    let point = LockPoint(point_of(&random32()?).map_err(err)?);
    Ok((point, point.id()))
}

/// An accept of an L→P quote, which has a gas drop, so it names where to.
fn accept_body(quote_event: String, point: LockPoint) -> Res<Body> {
    let key = keypair_from(&random32()?)?;
    Ok(Body::RfqAccept(Accept {
        quote_event,
        lock_point: point.to_hex(),
        taker_key: msg::hex(&key.public_key().serialize()),
        blinding_key: msg::hex(&random32()?),
        taker_polygon: TAKER.into(),
        gas_to: Some(fresh_address()?.checksummed()),
    }))
}

const QUIET: Duration = Duration::from_secs(3);
/// Longer than a session request takes when it is answered, so silence means refused and not slow.
const SESSION_WAIT: Duration = Duration::from_secs(10);

/// One L→P swap up to the maker's claim offer, with the taker's coin confirmed and nothing signed.
struct UpToOffer {
    relay: Relay,
    me: Keys,
    negotiation: String,
    swap: BotSwap,
    secret: [u8; 32],
    offer: msg::ClaimOffer,
}

fn up_to_offer(w: &World) -> Res<UpToOffer> {
    let (mut relay, me, negotiation, swap, secret) = l2p_agreed(w, 0)?;
    let got = wait_from(&mut relay, &me, &w.maker_nostr, &negotiation, NETWORK, Duration::from_secs(120))?;
    let Body::ClaimOffer(offer) = got.envelope.body else {
        return Err(format!("expected a claim offer, got {:?}", got.envelope.body));
    };
    Ok(UpToOffer { relay, me, negotiation, swap, secret, offer })
}

/// An L→P swap agreed and paid, `short` units under the agreed amount: a wrong coin, when it isn't 0.
fn l2p_agreed(w: &World, short: u64) -> Res<(Relay, Keys, String, BotSwap, [u8; 32])> {
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay_url, Duration::from_millis(200))?;
    let (key, chosen, secret) = fresh_swap_terms(Direction::L2P, 0, 0, 0, 0)?;
    let gas_to = fresh_address()?;
    let chain = w.chain.clone();
    let Negotiated { id: negotiation, terms, .. } = negotiate(
        &mut relay,
        &me,
        &w.maker_nostr,
        &taker_rules(w),
        l2p_request(),
        side_of(&chosen, Some(&gas_to)),
        move || chain.tip(),
        Duration::from_secs(15),
    )?;
    let (agreed, maker_key) = agreed_terms(&SwapTerms { gas_to: Some(gas_to.checksummed()), ..chosen }, &terms)?;
    taker_pays_to(w, &agreed.address(&maker_key)?, usdt(), terms.liquid_amount.0 - short)?;
    Ok((relay, me, negotiation, BotSwap { terms: agreed, key, maker_key }, secret))
}

type RefundParts = (EarlyRefund, LiquidTx, Vec<simplicityhl::elements::TxOut>);

/// The refund the taker would ask for, with its coins in the right order.
fn refund_tx(w: &World, s: &BotSwap) -> Res<RefundParts> {
    refund_of(w, s, s.terms.liquid_amount)
}

/// The refund of the USDt coin of this value at the swap's address.
fn refund_of(w: &World, s: &BotSwap, value: u64) -> Res<RefundParts> {
    let blinding = s.terms.blinding()?;
    let coins = w.chain.secret_coins(&s.terms.address(&s.maker_key)?, &blinding)?;
    let coin = exact_secret_coin(&coins, usdt(), value, w.chain.tip()?, 0).ok_or("no coin of ours")?;
    let fee_address = taker_fee_address(&s.terms.taker_key.0, &blinding)?;
    let fee = w.wait_for("the refund's fee coin", 60, || {
        let found = w.chain.secret_coins(&fee_address, &blinding)?;
        Ok(found.into_iter().find(|c| c.coin.height.is_some()))
    })?;
    let (tx, utxos) = build_swap_spend(&coin, &fee, &w.taker_wallet.address(), 0, Sequence::MAX)?;
    let started = EarlyRefund::start_with(&s.key, &s.terms.output(&s.maker_key)?, tx.clone(), utxos.clone(), w.genesis)?;
    Ok((started, tx, utxos))
}

/// The next answer about an early refund. The maker repeats its claim offer until it is answered, so those are skipped, as a
/// taker would.
fn early_answer(relay: &mut Relay, me: &Keys, maker: &nostr::prelude::PublicKey, negotiation: &str) -> Res<wrap::Received> {
    let start = Instant::now();
    loop {
        let left = Duration::from_secs(30).saturating_sub(start.elapsed());
        let got = wait_from(relay, me, maker, negotiation, NETWORK, left)?;
        if !matches!(got.envelope.body, Body::ClaimOffer(_)) {
            return Ok(got);
        }
    }
}

/// A maker who agrees to a refund must never lock, and one who has locked must never agree: either order would let the
/// taker keep both sides. Everything else here is a request it should turn down, without that costing the real taker.
fn rfq_early_refund(w: &mut World) -> Res<()> {
    let mut s = up_to_offer(w)?;
    let id = s.swap.terms.swap_id();
    let answer = answer_offer(&w.chain, &s.swap, &s.offer, w.genesis)?;
    let fee_address = taker_fee_address(&s.swap.terms.taker_key.0, &s.swap.terms.blinding()?)?;
    taker_pays_to(w, &fee_address, lbtc(), FEE + 1)?;
    let (started, tx, utxos) = refund_tx(w, &s.swap)?;
    let lock_point = s.offer.lock_point.clone();
    let ask = |tx: &LiquidTx, nonce: &liq2any_swapkey::Nonce| {
        Body::EarlyRefundAsk(msg::EarlyRefundAsk {
            lock_point: lock_point.clone(),
            tx: serialize_hex(tx),
            nonce: msg::hex(&nonce_bytes(nonce)),
        })
    };

    // Someone else can't ask for it.
    let mut stranger = RawTaker::new(w)?;
    stranger.id = s.negotiation.clone();
    stranger.send(ask(&tx, &started.nonce()))?;
    check(stranger.answer(QUIET)?.is_none(), "a stranger's request gets no answer")?;

    // A request that doesn't spend this swap's coin as its first input gets no signature, and spoils nothing.
    let mut swapped = tx.clone();
    swapped.input.swap(0, 1);
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, ask(&swapped, &started.nonce()))?;
    let got = early_answer(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation);
    check(got.is_err_and(|e| e.contains("declined")), "a request spending another coin first is declined")?;
    check(matches!(w.state(&id)?, State::WatchLock), "and the swap goes on")?;

    // Cancelling comes first: the maker promises never to lock before the taker has a fee coin or an address.
    let cancel = Body::CancelAsk(msg::CancelAsk { lock_point: lock_point.clone() });
    stranger.send(cancel.clone())?;
    check(stranger.answer(QUIET)?.is_none(), "a stranger can't cancel")?;
    check(matches!(w.state(&id)?, State::WatchLock), "and the swap goes on")?;
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, cancel)?;
    let got = early_answer(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation)?;
    check(matches!(got.envelope.body, Body::Cancelled(_)), "the maker cancels")?;
    w.wait_state(&id, "the maker dropping the swap", 30, |state| matches!(state, State::Abandoned))?;

    // The refund, asked for afterwards.
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, ask(&tx, &started.nonce()))?;
    let got = early_answer(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation)?;
    let Body::EarlyRefundShare(share) = got.envelope.body else {
        return Err(format!("expected the maker's share, got {:?}", got.envelope.body));
    };
    let maker_nonce = nonce_from(&unhex(&share.nonce)?).map_err(err)?;
    let maker_share = share_from(&unhex(&share.share)?).map_err(err)?;

    // A taker can't be fooled by a bad share, which would leave it publishing something invalid.
    let output = s.swap.terms.output(&s.swap.maker_key)?;
    let other = EarlyRefund::start_with(&s.swap.key, &output, tx.clone(), utxos.clone(), w.genesis)?;
    let wrong = share_from(&[7u8; 32]).map_err(err)?;
    check(other.finish(&s.swap.key, &output, &s.swap.maker_key.0, &maker_nonce, wrong).is_err(), "a bad share is caught")?;

    let (mut refund, spent) = started.finish(&s.swap.key, &output, &s.swap.maker_key.0, &maker_nonce, maker_share)?;
    sign_keypath(&mut refund, &spent, 1, &s.swap.key, w.genesis)?;
    w.chain.broadcast(&refund)?;
    let address = s.swap.terms.address(&s.swap.maker_key)?;
    let blinding = s.swap.terms.blinding()?;
    w.wait_for("the refund to confirm", 60, || Ok(w.chain.secret_coins(&address, &blinding)?.is_empty().then_some(())))?;
    check(refund.input[0].witness.script_witness.len() == 1, "on-chain it is one signature, like a claim")?;
    w.wait_state(&id, "the maker dropping the swap", 30, |state| matches!(state, State::Abandoned))?;

    // Too late for the adapter signature, which would be the way to a lock.
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, Body::ClaimAnswer(answer))?;
    std::thread::sleep(QUIET * 3);
    check(w.evm.lock_made(&w.maker_lock_id(&s.swap.terms.lock_point))?.is_none(), "having agreed, the maker never locks")?;

    // The other order: once it has locked, it can't agree.
    let mut s = up_to_offer(w)?;
    let id = s.swap.terms.swap_id();
    let fee_address = taker_fee_address(&s.swap.terms.taker_key.0, &s.swap.terms.blinding()?)?;
    taker_pays_to(w, &fee_address, lbtc(), FEE + 1)?;
    let (started, tx, _) = refund_tx(w, &s.swap)?;
    let answer = answer_offer(&w.chain, &s.swap, &s.offer, w.genesis)?;
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, Body::ClaimAnswer(answer))?;
    let lock = w.maker_lock_id(&s.swap.terms.lock_point);
    w.wait_for("the maker to lock on Polygon", 120, || w.evm.lock_made(&lock))?;
    let late = Body::EarlyRefundAsk(msg::EarlyRefundAsk {
        lock_point: s.offer.lock_point.clone(),
        tx: serialize_hex(&tx),
        nonce: msg::hex(&nonce_bytes(&started.nonce())),
    });
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, late)?;
    let got = early_answer(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation);
    check(got.is_err_and(|e| e.contains("declined")), "having locked, the maker refuses")?;
    let cancel = Body::CancelAsk(msg::CancelAsk { lock_point: s.offer.lock_point.clone() });
    send_body(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation, NETWORK, cancel)?;
    let got = early_answer(&mut s.relay, &s.me, &w.maker_nostr, &s.negotiation);
    check(got.is_err_and(|e| e.contains("declined")), "and won't cancel either")?;
    w.taker_claim_polygon(&lock, &s.secret)?;
    w.wait_state(&id, "the maker completing the swap", 120, done(Outcome::Completed))?;
    Ok(())
}

/// P→L, before the maker locks on Liquid: the taker asks for its lock back, and the maker releases it, which the escrow can
/// only pay to the lock's own initiator. A deadline too close for the maker's gate keeps the maker from locking meanwhile.
fn rfq_p2l_release(w: &mut World) -> Res<()> {
    let mut a = p2l_agreed(w)?;
    let id = a.chosen.swap_id();
    let before = w.taker_usdt0()?;
    let lock = p2l_lock(w, &a, P2L_GATE_SECS - 30)?;
    w.wait_for("the lock to finalize", 30, || {
        Ok(Some(w.evm.rpc.lock_at(&w.evm.escrow, &lock, "finalized")?).filter(|l| l.state == LockState::Open))
    })?;

    // Someone else can't ask for it.
    let mut stranger = RawTaker::new(w)?;
    stranger.id = a.negotiation.clone();
    stranger.send(release_ask(&a))?;
    check(stranger.answer(QUIET)?.is_none(), "a stranger's request gets no answer")?;
    check(w.evm.lock(&lock)?.state == LockState::Open, "and the lock stays")?;

    let ask = release_ask(&a);
    send_body(&mut a.relay, &a.me, &w.maker_nostr, &a.negotiation, NETWORK, ask)?;
    w.wait_for("the maker to release the lock", 60, || Ok((w.evm.lock(&lock)?.state == LockState::Refunded).then_some(())))?;
    check(w.taker_usdt0()? == before, "the taker has all of it back")?;
    w.wait_state(&id, "the maker dropping the swap", 30, |state| matches!(state, State::Abandoned))?;
    let (agreed, maker_key) = agreed_terms(&a.chosen, &a.terms)?;
    std::thread::sleep(QUIET * 3);
    let coins = w.chain.secret_coins(&agreed.address(&maker_key)?, &agreed.blinding()?)?;
    check(coins.is_empty(), "having released, the maker never locks on Liquid")?;

    // Asking again changes nothing.
    let ask = release_ask(&a);
    send_body(&mut a.relay, &a.me, &w.maker_nostr, &a.negotiation, NETWORK, ask)?;
    let got = early_answer(&mut a.relay, &a.me, &w.maker_nostr, &a.negotiation);
    check(got.is_err_and(|e| e.contains("declined")), "a second request is declined")?;
    Ok(())
}

/// P→L, before the taker locks: the taker cancels, and the maker gives the swap up at once, rather than at the end of its lock
/// window, which frees the Liquid it held for it. Once the taker has locked, a cancel is refused and the swap goes on.
fn rfq_p2l_cancel(w: &mut World) -> Res<()> {
    let cancel = |a: &P2lAgreed| Body::CancelAsk(msg::CancelAsk { lock_point: a.chosen.lock_point.to_hex() });

    let mut a = p2l_agreed(w)?;
    let id = a.chosen.swap_id();
    let ask = cancel(&a);
    send_body(&mut a.relay, &a.me, &w.maker_nostr, &a.negotiation, NETWORK, ask)?;
    let got = early_answer(&mut a.relay, &a.me, &w.maker_nostr, &a.negotiation)?;
    check(matches!(got.envelope.body, Body::Cancelled(_)), "the maker confirms the cancel")?;
    w.wait_state(&id, "the maker dropping the swap", 30, |state| matches!(state, State::Abandoned))?;

    // The taker has locked: the cancel is refused, and the swap is still on. A deadline too close for the maker's gate keeps it
    // from locking on Liquid while the test asks.
    let mut b = p2l_agreed(w)?;
    let id = b.chosen.swap_id();
    let lock = p2l_lock(w, &b, P2L_GATE_SECS - 30)?;
    let ask = cancel(&b);
    send_body(&mut b.relay, &b.me, &w.maker_nostr, &b.negotiation, NETWORK, ask)?;
    let got = early_answer(&mut b.relay, &b.me, &w.maker_nostr, &b.negotiation);
    check(got.is_err_and(|e| e.contains("locked already")), "having locked, the taker can't cancel")?;
    check(matches!(w.state(&id)?, State::WatchLock), "and the maker still has the swap")?;

    // Tidy up: the taker asks for its lock back, and the maker releases it.
    let ask = release_ask(&b);
    send_body(&mut b.relay, &b.me, &w.maker_nostr, &b.negotiation, NETWORK, ask)?;
    w.wait_for("the maker to release the lock", 60, || Ok((w.evm.lock(&lock)?.state == LockState::Refunded).then_some(())))?;
    Ok(())
}

/// A coin of the wrong amount: the maker never locks for it, and can still hand it back early, through the same cancel.
fn rfq_early_refund_wrong_coin(w: &mut World) -> Res<()> {
    const SHORT: u64 = 100;
    let (mut relay, me, negotiation, swap, _) = l2p_agreed(w, SHORT)?;
    let id = swap.terms.swap_id();
    let lock_point = swap.terms.lock_point.to_hex();
    let address = swap.terms.address(&swap.maker_key)?;
    let blinding = swap.terms.blinding()?;
    let wrong = swap.terms.liquid_amount - SHORT;
    w.wait_for("the wrong coin to confirm", 60, || {
        Ok(exact_secret_coin(&w.chain.secret_coins(&address, &blinding)?, usdt(), wrong, w.chain.tip()?, 1))
    })?;
    std::thread::sleep(QUIET * 2);
    check(w.evm.lock_made(&w.maker_lock_id(&swap.terms.lock_point))?.is_none(), "the maker doesn't lock for a coin of the wrong amount")?;

    let cancel = Body::CancelAsk(msg::CancelAsk { lock_point: lock_point.clone() });
    send_body(&mut relay, &me, &w.maker_nostr, &negotiation, NETWORK, cancel)?;
    let got = early_answer(&mut relay, &me, &w.maker_nostr, &negotiation)?;
    check(matches!(got.envelope.body, Body::Cancelled(_)), "the maker cancels")?;
    w.wait_state(&id, "the maker dropping the swap", 30, |state| matches!(state, State::Abandoned))?;

    let fee_address = taker_fee_address(&swap.terms.taker_key.0, &blinding)?;
    taker_pays_to(w, &fee_address, lbtc(), FEE + 1)?;
    let (started, tx, utxos) = refund_of(w, &swap, wrong)?;
    let ask = Body::EarlyRefundAsk(msg::EarlyRefundAsk {
        lock_point,
        tx: serialize_hex(&tx),
        nonce: msg::hex(&nonce_bytes(&started.nonce())),
    });
    send_body(&mut relay, &me, &w.maker_nostr, &negotiation, NETWORK, ask)?;
    let got = early_answer(&mut relay, &me, &w.maker_nostr, &negotiation)?;
    let Body::EarlyRefundShare(share) = got.envelope.body else {
        return Err(format!("expected the maker's share, got {:?}", got.envelope.body));
    };
    let maker_nonce = nonce_from(&unhex(&share.nonce)?).map_err(err)?;
    let maker_share = share_from(&unhex(&share.share)?).map_err(err)?;
    let output = swap.terms.output(&swap.maker_key)?;
    let (mut refund, spent) = started.finish(&swap.key, &output, &swap.maker_key.0, &maker_nonce, maker_share)?;
    sign_keypath(&mut refund, &spent, 1, &swap.key, w.genesis)?;
    check(spent == utxos, "the refund spends the wrong coin and the fee coin")?;
    w.chain.broadcast(&refund)?;
    w.wait_for("the refund to confirm", 60, || Ok(w.chain.secret_coins(&address, &blinding)?.is_empty().then_some(())))?;
    check(swap.terms.lock_height > w.chain.tip()?, "the wrong coin came back early, long before the lock height")?;
    Ok(())
}

/// E6 and E7 against the real daemon: each of these is a message the maker must not act on.
fn rfq_attacks(w: &mut World) -> Res<()> {
    let request = || Body::RfqRequest(l2p_request());
    let known = |w: &World, hash: &[u8; 32]| w.store.state(hash).map(|s| s.is_some()).map_err(err);

    // Stale and wrong-network messages get no answer at all.
    let mut t = RawTaker::new(w)?;
    let id = t.id.clone();
    t.send_at(&id, NETWORK, now_secs() - 600, request())?;
    check(t.answer(QUIET)?.is_none(), "a stale request gets no answer")?;
    t.send_at(&id, "testnet", now_secs(), request())?;
    check(t.answer(QUIET)?.is_none(), "a request for another network gets no answer")?;

    // A pair that isn't Liquid to Polygon or back is refused out loud.
    let mut odd_pair = RawTaker::new(w)?;
    odd_pair.send(Body::RfqRequest(rfq_request(Chain::Polygon, Chain::Polygon)))?;
    let answer = odd_pair.answer(Duration::from_secs(10))?.ok_or("no answer to a bad pair")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "a request for Polygon to Polygon is declined")?;

    // A stranger can't accept someone else's quote.
    let mut victim = RawTaker::new(w)?;
    victim.send(request())?;
    let (quote, quote_id) = victim.quote(Duration::from_secs(10))?;
    check(quote.gas_drop == Wei(GAS_DROP), "an L→P quote carries the gas drop")?;
    let mut mallory = RawTaker::new(w)?;
    let (point, hash) = fresh_id()?;
    let stolen = accept_body(quote_id.clone(), point)?;
    let vid = victim.id.clone();
    mallory.send_at(&vid, NETWORK, now_secs(), stolen)?;
    check(mallory.answer(QUIET)?.is_none(), "a stranger accepting someone's quote gets no answer")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // An accept naming the wrong quote is refused.
    let (point, hash) = fresh_id()?;
    victim.send(accept_body("not-the-quote".into(), point)?)?;
    check(victim.answer(QUIET)?.is_none(), "an accept naming the wrong quote gets no answer")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // An accept with nowhere for the gas drop, or a taker address that is the maker's own or not an address, is refused.
    type Spoil = fn(Accept) -> Accept;
    let spoiled: [(&str, Spoil); 4] = [
        ("no gas_to for a gas drop", |a| Accept { gas_to: None, ..a }),
        ("a taker address that is the maker's own", |a| Accept { taker_polygon: "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC".into(), ..a }),
        ("a taker address with a bad checksum", |a| Accept { taker_polygon: "0x70997970C51812dc3A010C7d01b50e0d17dc79c8".into(), ..a }),
        ("the zero address", |a| Accept { taker_polygon: ZERO.into(), ..a }),
    ];
    for (what, spoil) in spoiled {
        let mut t = RawTaker::new(w)?;
        t.send(request())?;
        let (_, quote_id) = t.quote(Duration::from_secs(10))?;
        let (point, hash) = fresh_id()?;
        let Body::RfqAccept(good) = accept_body(quote_id, point)? else { unreachable!() };
        t.send(Body::RfqAccept(spoil(good)))?;
        let answered = t.answer(Duration::from_secs(5))?;
        check(answered.is_none_or(|a| matches!(a.envelope.body, Body::RfqReject(_))), &format!("an accept with {what} is refused"))?;
        check(!known(w, &hash)?, "and no swap was registered")?;
    }

    // A quote can't be accepted after it expires.
    let mut slow = RawTaker::new(w)?;
    slow.send(request())?;
    let (_, quote_id) = slow.quote(Duration::from_secs(10))?;
    std::thread::sleep(Duration::from_secs(QUOTE_SECS + 2));
    let (point, hash) = fresh_id()?;
    slow.send(accept_body(quote_id, point)?)?;
    check(slow.answer(QUIET)?.is_none(), "an expired quote can't be accepted")?;
    check(!known(w, &hash)?, "and no swap was registered")?;

    // The same accept delivered twice gets the same answer, and only one swap.
    let mut twice = RawTaker::new(w)?;
    twice.send(request())?;
    let (_, quote_id) = twice.quote(Duration::from_secs(10))?;
    let (point, hash) = fresh_id()?;
    let body = accept_body(quote_id, point)?;
    let rumor = twice.send(body)?;
    let first = twice.answer(Duration::from_secs(10))?.ok_or("no terms")?;
    let Body::RfqTerms(MakerTerms { accept_event, .. }) = &first.envelope.body else {
        return Err("expected terms".into());
    };
    check(*accept_event == rumor, "the terms name this accept")?;
    w.relay.replay_everything();
    std::thread::sleep(QUIET);
    let swaps = w.store.open_swaps().map_err(err)?.iter().filter(|(t, _)| t.swap_id() == hash).count();
    check(swaps == 1, "a relay replaying everything creates no second swap")?;

    // A point that isn't on the curve can't be locked to or claimed, so no swap is made of it.
    let mut off_curve = RawTaker::new(w)?;
    off_curve.send(request())?;
    let (_, quote_id) = off_curve.quote(Duration::from_secs(10))?;
    let Body::RfqAccept(good) = accept_body(quote_id, fresh_id()?.0)? else { unreachable!() };
    let not_a_point = format!("02{}", "ff".repeat(32));
    off_curve.send(Body::RfqAccept(Accept { lock_point: not_a_point.clone(), ..good }))?;
    let answer = off_curve.answer(Duration::from_secs(10))?.ok_or("no answer")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "a lock point off the curve is declined")?;
    check(!known(w, &LockPoint::from_hex(&not_a_point).ok_or("hex")?.id())?, "and no swap was registered")?;

    // A point can never be reused: a new negotiation offering the same point is declined.
    let mut again = RawTaker::new(w)?;
    again.send(request())?;
    let (_, quote_id) = again.quote(Duration::from_secs(10))?;
    again.send(accept_body(quote_id, point)?)?;
    let answer = again.answer(Duration::from_secs(10))?.ok_or("no answer")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "a reused lock point is declined")?;

    // An amount it won't quote gets a refusal, so the taker needn't wait for a timeout.
    let mut too_big = RawTaker::new(w)?;
    let mut over_limit = l2p_request();
    over_limit.amount = Units(RFQ_MAX * 2);
    too_big.send(Body::RfqRequest(over_limit))?;
    let answer = too_big.answer(Duration::from_secs(10))?.ok_or("no answer to an amount over the limit")?;
    check(matches!(answer.envelope.body, Body::RfqReject(_)), "an amount over the limit is declined out loud")?;

    // Paused by the operator: new requests are declined, saying so, and a quote given before the pause starts nothing after it.
    let mut before = RawTaker::new(w)?;
    before.send(request())?;
    let (_, quote_id) = before.quote(Duration::from_secs(10))?;
    w.store.set_paused(true).map_err(err)?;
    let mut during = RawTaker::new(w)?;
    during.send(request())?;
    let answer = during.answer(Duration::from_secs(10))?.ok_or("no answer while paused")?;
    check(matches!(&answer.envelope.body, Body::RfqReject(r) if r.reason.contains("paused")), "a paused maker declines a request, saying so")?;
    let (point, hash) = fresh_id()?;
    before.send(accept_body(quote_id, point)?)?;
    let answer = before.answer(Duration::from_secs(10))?.ok_or("no answer to an accept while paused")?;
    check(matches!(&answer.envelope.body, Body::RfqReject(r) if r.reason.contains("paused")), "an earlier quote can't be accepted while paused")?;
    check(!known(w, &hash)?, "and no swap was registered")?;
    w.store.set_paused(false).map_err(err)?;
    let mut after = RawTaker::new(w)?;
    after.send(request())?;
    after.quote(Duration::from_secs(10))?;
    println!("    ok: paused, the maker declines new swaps and earlier quotes, and quotes again once resumed");

    // A maker's message types from a taker are ignored.
    let mut odd = RawTaker::new(w)?;
    let fake_quote = msg::Quote { send: Units(1), receive: Units(1), quote_expiry: 0, taker_lock_window: 0, gas_drop: Wei(0) };
    odd.send(Body::RfqQuote(fake_quote))?;
    check(odd.answer(QUIET)?.is_none(), "a quote sent to the maker gets no answer")?;

    // A flood from one key is cut off, and others are still served.
    let mut flood = RawTaker::new(w)?;
    let mut answered = 0;
    for _ in 0..FLOOD_LIMIT + 10 {
        flood.id = msg::hex(&random32()?);
        flood.send(request())?;
    }
    while flood.answer(Duration::from_secs(2))?.is_some() {
        answered += 1;
    }
    check(answered <= FLOOD_LIMIT, "a flood from one key is rate limited")?;
    let mut other = RawTaker::new(w)?;
    other.send(request())?;
    check(other.quote(Duration::from_secs(10)).is_ok(), "and other takers are still served")?;

    // A relay dropping everything costs time, and moves no funds.
    w.relay.drop_all(true);
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay.url(), Duration::from_millis(200))?;
    let (point, hash) = fresh_id()?;
    let side = TakerSide {
        lock_point: point.0,
        taker_key: keypair_from(&random32()?)?.public_key().serialize(),
        blinding_key: random32()?,
        taker_polygon: TAKER.into(),
        gas_to: Some(fresh_address()?.checksummed()),
    };
    let chain = w.chain.clone();
    let maker = w.maker_nostr;
    let rules = taker_rules(w);
    let dropped = negotiate(&mut relay, &me, &maker, &rules, l2p_request(), side, move || chain.tip(), QUIET);
    w.relay.drop_all(false);
    check(dropped.is_err(), "the taker gives up cleanly when a relay drops everything")?;
    check(!known(w, &hash)?, "and no swap exists")?;

    // Forgery of the maker: a taker ignores answers from anyone but the maker it chose.
    let me = Keys::generate();
    let mut relay = Relay::connect(&w.relay.url(), Duration::from_millis(200))?;
    relay.subscribe("forge", &[Kind::from(msg::WRAP_KIND)], Some(&me.public_key()), now_secs() - 2 * 86_400)?;
    let forger = Keys::generate();
    let fake = Envelope {
        v: msg::VERSION,
        id: msg::hex(&[1; 32]),
        network: NETWORK.into(),
        sent_at: now_secs(),
        body: Body::RfqQuote(msg::Quote {
            send: Units(RFQ_SEND),
            receive: Units(RFQ_SEND),
            quote_expiry: now_secs() + 60,
            taker_lock_window: 30,
            gas_drop: Wei(GAS_DROP),
        }),
    };
    let (event, _) = wrap::wrap(&forger, &me.public_key(), &fake, now_secs()).map_err(|e| format!("{e:?}"))?;
    w.relay.inject(event);
    let start = Instant::now();
    let mut seen = None;
    while start.elapsed() < QUIET && seen.is_none() {
        if let Some(RelayEvent::Event(e)) = relay.next_event()? {
            seen = wrap::unwrap(&me, &e).ok();
        }
    }
    let got = seen.ok_or("the forged message never arrived")?;
    check(
        got.sender == forger.public_key() && got.sender != w.maker_nostr,
        "a forged quote is attributed to its real author, so a taker can't mistake it",
    )?;
    Ok(())
}

#[test]
#[ignore = "needs elementsd and Foundry; see the module docs"]
fn the_daemon_runs_real_swaps() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut world = build_world(&root);
    type Scenario = (&'static str, fn(&mut World) -> Res<()>);
    let scenarios: Vec<Scenario> = vec![
        ("L2P with crashes and a gas drop (E1, E17)", l2p_with_crashes),
        ("P2L with crashes (E1, E17)", p2l_with_crashes),
        ("crash between commit and send (E17)", crash_between_commit_and_send),
        ("L2P taker vanishes (E3)", l2p_taker_vanishes),
        ("L2P taker never signs", l2p_taker_never_signs),
        ("P2L taker vanishes (E3)", p2l_taker_vanishes),
        ("bad locks (E9)", bad_locks),
        ("Liquid halt extends (E11)", liquid_halt_extends),
        ("Polygon finality stall", polygon_stall_holds_locks),
        ("blocked addresses", blocked_addresses),
        ("rollback (E15)", rollback_stops_everything),
        ("RFQ L2P swap (E1)", rfq_l2p_swap),
        ("RFQ P2L swap (E1)", rfq_p2l_swap),
        ("RFQ attacks (E6, E7)", rfq_attacks),
        ("RFQ early refund", rfq_early_refund),
        ("RFQ wrong coin refunded early", rfq_early_refund_wrong_coin),
        ("RFQ P2L release", rfq_p2l_release),
        ("RFQ P2L cancel", rfq_p2l_cancel),
    ];
    let filter = std::env::var("SCENARIO").unwrap_or_default();
    let mut failures = vec![];
    for (name, run) in scenarios {
        if !name.contains(&filter) {
            continue;
        }
        println!("\n==> {name}");
        let start = Instant::now();
        match run(&mut world) {
            Ok(()) => println!("PASS {name} ({}s)", start.elapsed().as_secs()),
            Err(e) => {
                println!("FAIL {name}: {e}");
                failures.push(name);
            }
        }
        if world.daemon.is_none() {
            world.start_daemon();
        }
        // A scenario that failed midway may leave a chain halted.
        world.miner.resume();
        let _ = world.evm.halt(false);
    }
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "failed: {failures:?}");
}
