//! The real signer binary, attacked through its socket as a compromised engine would. Liquid is a regtest node, and Polygon a
//! local `anvil` chain with the real escrow and the stand-in token deployed on it, so everything the signer reads is real.
//! From liq2sol's, with Polygon in Solana's place.
//!
//!   ELEMENTSD=~/bin/elementsd ELEMENTS_CLI=~/bin/elements-cli cargo test -p liq2any-signer --test process -- --ignored --nocapture
//!
//! Needs Foundry (`anvil`, `forge`, `cast`) on the PATH. Test networks only: anvil's well-known keys hold nothing real.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use liq2any_chain::esplora::{build_confidential_pay, Esplora, Rpc};
use liq2any_chain::evm::{decode_signed, from_hex, Address as EvmAddress};
use liq2any_chain::liquid::{
    build_swap_spend, exact_secret_coin, is_key_path_spend, keypair_from, lbtc, published_signature, random32, usdt,
    use_network, NetworkConfig, Payment, SecretCoin, Wallet, FEE,
};
use liq2any_chain::polygon::{lock_id, Lock, Rpc as PolygonRpc, State as LockState};
use liq2any_chain::swap::{answer_claim, TakerClaim};
use liq2any_maker_core::engine::{Config as EngineConfig, Direction};
use liq2any_maker_core::policy::{Limits, Refusal};
use liq2any_maker_core::LockPoint;
use liq2any_signer::{
    unhex, Asset, Call, Chain, Client, Config, LiquidSource, PolygonConfig, Reply, SwapTerms, WithdrawTo, MAX_REQUEST_BYTES,
};
use liq2any_swapkey::{
    nonce_bytes, nonce_from, point_of, polygon_address_of, polygon_value, secret_from_polygon, share_bytes, share_from,
    Nonce,
};
use simplicityhl::elements::encode::{deserialize, serialize_hex};
use simplicityhl::elements::secp256k1_zkp::SecretKey;
use simplicityhl::elements::{Address, AddressParams, AssetId, BlockHash, OutPoint, Script, Sequence, Transaction as LiquidTx};

const USDT0: u64 = 10;
const LIQ_AMOUNT: u64 = USDT0 * 100;

/// Anvil's default accounts: #0 deploys and blocks (Tether's role on the stand-in), #1 is the taker, #2 the signer's key.
const DEPLOYER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const TAKER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const TAKER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const SIGNER_KEY: &str = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
/// Anvil's #3, a stranger.
const STRANGER: &str = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";

struct Node {
    child: Child,
    rpc: Rpc,
    dir: PathBuf,
}

impl Node {
    fn start(root: &Path) -> Self {
        let daemon = std::env::var("ELEMENTSD").unwrap_or_else(|_| "elementsd".into());
        let cli = std::env::var("ELEMENTS_CLI").unwrap_or_else(|_| "elements-cli".into());
        let port = 40_000 + (std::process::id() % 20_000) as u16;
        let dir = root.join("node");
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(daemon)
            .arg(format!("-datadir={}", dir.display()))
            .args(["-chain=liquidregtest", "-listen=0", "-txindex=1", "-validatepegin=0", "-fallbackfee=0.00001"])
            .args(["-initialfreecoins=2100000000", "-anyonecanspendaremine=1", "-evbparams=simplicity:-1:::"])
            .arg("-acceptdiscountct=1")
            .args(["-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}"), "-printtoconsole=0"])
            .stdout(Stdio::null())
            .spawn()
            .expect("start elementsd");
        let base: Vec<String> =
            ["-chain=liquidregtest", "-rpcuser=t", "-rpcpassword=t", &format!("-rpcport={port}")].map(String::from).to_vec();
        let rpc = Rpc { cli, args: base };
        let start = Instant::now();
        while rpc.call(&["getblockcount"]).is_err() {
            if start.elapsed() > Duration::from_secs(90) || matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("elementsd did not start");
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let node = Node { child, rpc, dir };
        node.rpc.call(&["createwallet", "w"]).unwrap();
        node.rpc.call(&["rescanblockchain"]).unwrap();
        node
    }

    fn mine(&self, blocks: u32) {
        let address = self.rpc.call(&["getnewaddress"]).unwrap();
        self.rpc.call(&["generatetoaddress", &blocks.to_string(), &address]).unwrap();
    }

    fn fund(&self, address: &str, sats: u64, label: &str) {
        let amount = format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000);
        self.rpc.call(&["sendtoaddress", address, &amount, "", "", "false", "false", "1", "UNSET", "false", label]).unwrap();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.rpc.call(&["stop"]);
        let start = Instant::now();
        while matches!(self.child.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A local EVM chain with the escrow and stand-in token deployed, and 2,100 blocks of history one second apart, so the
/// measured block time is 1,000 ms.
struct Evm {
    child: Child,
    url: String,
    rpc: PolygonRpc,
    escrow: EvmAddress,
    token: EvmAddress,
}

fn cast(args: &[&str]) -> String {
    let out = Command::new("cast").args(args).output().expect("cast");
    assert!(out.status.success(), "cast {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Evm {
    fn start() -> Self {
        let port = 45_000 + (std::process::id() % 15_000) as u16;
        let url = format!("http://127.0.0.1:{port}");
        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .spawn()
            .expect("start anvil");
        let rpc = PolygonRpc::new(&url);
        let start = Instant::now();
        while rpc.chain_id().is_err() {
            assert!(start.elapsed() < Duration::from_secs(30), "anvil did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        cast(&["rpc", "anvil_mine", "0x834", "0x1", "--rpc-url", &url]);
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
        let escrow =
            deploy(&["src/Escrow.sol:Escrow", "--constructor-args", &token.to_string(), "1209600", "604800"]);
        let evm = Evm { child, url, rpc, escrow, token };
        evm.finalize();
        evm
    }

    /// Mine past anvil's finality depth of 64 blocks, so what was just sent is in the finalized block the signer reads.
    fn finalize(&self) {
        cast(&["rpc", "anvil_mine", "0x41", "0x1", "--rpc-url", &self.url]);
    }

    fn send(&self, key: &str, to: &EvmAddress, sig: &str, args: &[&str]) {
        let mut all = vec!["send", "--rpc-url", &self.url, "--private-key", key];
        let to = to.to_string();
        all.push(&to);
        all.push(sig);
        all.extend_from_slice(args);
        cast(&all);
    }

    /// Broadcast what the signer signed, and wait for it to be mined.
    fn publish(&self, reply: &Reply) -> u64 {
        let Reply::Polygon { raw, nonce, hash } = reply else { panic!("expected a Polygon transaction, got {reply:?}") };
        self.rpc.send_raw(&from_hex(raw).unwrap()).unwrap();
        let hash: [u8; 32] = from_hex(hash).unwrap().try_into().unwrap();
        let start = Instant::now();
        loop {
            if let Some(r) = self.rpc.receipt(&hash).unwrap() {
                assert!(r.success, "the transaction at nonce {nonce} reverted");
                return *nonce;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "not mined");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn lock(&self, id: &[u8; 32]) -> Lock {
        self.rpc.lock_at(&self.escrow, id, "latest").unwrap()
    }

    fn now(&self) -> (u64, u64) {
        let b = self.rpc.block("latest").unwrap();
        (b.time, b.number)
    }

    /// The finalized block, which the signer reads its clock from. Anvil's trails its head by 64 blocks.
    fn finalized(&self) -> (u64, u64) {
        let b = self.rpc.block("finalized").unwrap();
        (b.time, b.number)
    }

    fn token_balance(&self, who: &str) -> u128 {
        self.rpc.token_balance(&self.token, &EvmAddress::parse(who).unwrap(), "latest").unwrap()
    }

    fn block(&self, who: &str, on: bool) {
        let sig = if on { "addToBlockedList(address)" } else { "removeFromBlockedList(address)" };
        self.send(DEPLOYER_KEY, &self.token.clone(), sig, &[who]);
    }
}

impl Drop for Evm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct SignerProcess {
    child: Child,
    config: PathBuf,
    socket: PathBuf,
}

impl SignerProcess {
    fn start(config: &Path, socket: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_liq2any-signer")).arg(config).spawn().expect("start signer");
        let start = Instant::now();
        while Client::connect(socket).is_err() {
            assert!(start.elapsed() < Duration::from_secs(30), "signer did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        Self { child, config: config.into(), socket: socket.into() }
    }

    fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        *self = Self::start(&self.config.clone(), &self.socket.clone());
    }
}

impl Drop for SignerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn liquid_tx(reply: Reply) -> LiquidTx {
    let Reply::Liquid(hex) = reply else { panic!("expected a Liquid transaction, got {reply:?}") };
    deserialize(&unhex(&hex).unwrap()).unwrap()
}

#[track_caller]
fn assert_refused(reply: Reply, expected: Refusal) {
    assert_eq!(reply, Reply::Refused(expected.clone()));
    println!("    ok: refused with {expected:?}");
}

#[track_caller]
fn assert_bad_timeout(reply: Reply) {
    assert!(matches!(reply, Reply::Refused(Refusal::BadTimeout(_))), "{reply:?}");
}

#[track_caller]
fn assert_failed(reply: Reply, containing: &str) {
    assert!(matches!(&reply, Reply::Failed(e) if e.contains(containing)), "expected a failure about {containing:?}, got {reply:?}");
}

#[test]
#[ignore = "needs elementsd and Foundry; see the module docs"]
fn the_signer_process_resists_a_compromised_engine() {
    let root = std::env::temp_dir().join(format!("liq2any-signer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let private = root.join("private");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();

    let node = Node::start(&root);
    node.mine(1);
    let issued: serde_json::Value = serde_json::from_str(&node.rpc.call(&["issueasset", "1000", "0", "false"]).unwrap()).unwrap();
    let usdt_id = issued["asset"].as_str().unwrap().to_string();
    node.mine(1);
    let info: serde_json::Value = serde_json::from_str(&node.rpc.call(&["getsidechaininfo"]).unwrap()).unwrap();
    let lbtc_id = info["pegged_asset"].as_str().unwrap().to_string();
    use_network(NetworkConfig {
        params: &AddressParams::ELEMENTS,
        usdt: AssetId::from_str(&usdt_id).unwrap(),
        lbtc: AssetId::from_str(&lbtc_id).unwrap(),
    });
    let chain = Esplora::Node(node.rpc.clone());
    let genesis = BlockHash::from_str(&chain.genesis().unwrap()).unwrap();

    let evm = Evm::start();
    println!("anvil at {}: escrow {}, token {}", evm.url, evm.escrow, evm.token);

    // The signer's Polygon key is anvil's account #2, so it holds test ETH from the start.
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("polygon.key"), from_hex(SIGNER_KEY).unwrap()).unwrap();

    let socket = private.join("signer.sock");
    let limits = Limits { extend_after_stall_secs: 3, ..Limits::from(&EngineConfig::default()) };
    let config = Config {
        network: "testnet".into(),
        socket: socket.clone(),
        ledger: root.join("signer.db"),
        keys_dir: keys.clone(),
        liquid: LiquidSource::Node { cli: node.rpc.cli.clone(), args: node.rpc.args.clone() },
        polygon: PolygonConfig {
            rpc: evm.url.clone(),
            escrow: Some(evm.escrow.to_string()),
            token: Some(evm.token.to_string()),
            fees: Default::default(),
        },
        limits,
        regtest_assets: Some((usdt_id.clone(), lbtc_id.clone())),
        fee_coin_sats: FEE,
        fresh_addresses: false,
        withdraw_to: WithdrawTo::default(),
    };
    let config_path = root.join("signer.json");
    std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

    println!("\n==> Startup checks");
    let refuses = |name: &str, cfg: &Config, why: &str| {
        let path = root.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_string(cfg).unwrap()).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_liq2any-signer")).arg(&path).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains(why), "{name}: {err}");
        println!("    ok: refuses to start: {why}");
    };
    // A shared directory must be refused, because the socket would be briefly open to everyone.
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    refuses("open", &Config { socket: shared.join("signer.sock"), ..config.clone() }, "must be private");
    let api = Config {
        fresh_addresses: true,
        liquid: LiquidSource::Esplora("https://blockstream.info/liquidtestnet/api".into()),
        ..config.clone()
    };
    refuses("api", &api, "fresh_addresses needs liquid to be our own node");
    let poly = |escrow: Option<String>, token: Option<String>| Config {
        polygon: PolygonConfig { escrow, token, ..config.polygon.clone() },
        ..config.clone()
    };
    refuses("no-escrow", &poly(Some(STRANGER.into()), Some(evm.token.to_string())), "no escrow contract");
    refuses("half", &poly(Some(evm.escrow.to_string()), None), "or neither");
    refuses(
        "mainnet-token",
        &poly(Some(evm.escrow.to_string()), Some("0xc2132D05D31c914a87C6611C10748AEb04B58e8F".into())),
        "mainnet's USDT0",
    );
    refuses("mainnet", &Config { network: "mainnet".into(), regtest_assets: None, ..config.clone() }, "refusing to start");

    let mut signer = SignerProcess::start(&config_path, &socket);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket) & 0o077, 0, "socket private");
    for key in ["swap-seed.key", "liquid-wallet.key", "liquid-blinding.key"] {
        assert_eq!(mode(&keys.join(key)), 0o600, "{key}");
    }
    println!("    ok: socket and key files are private");

    let mut client = Client::connect(&socket).unwrap();
    let Reply::Identity { liquid_address, polygon, .. } = client.call(&Call::Identity).unwrap() else { panic!() };
    assert_eq!(polygon, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC", "anvil's account #2, from the key file");
    node.fund(&liquid_address, 500_000, "bitcoin");
    node.fund(&liquid_address, 30 * LIQ_AMOUNT, &usdt_id);
    evm.send(SIGNER_KEY, &evm.token, "faucet(uint256)", &["1000000000"]);
    evm.send(TAKER_KEY, &evm.token, "faucet(uint256)", &["1000000000"]);
    evm.send(TAKER_KEY, &evm.token, "approve(address,uint256)", &[&evm.escrow.to_string(), &u128::MAX.to_string()]);

    let taker = Wallet {
        key: keypair_from(&random32().unwrap()).unwrap(),
        blinding: SecretKey::from_slice(&random32().unwrap()).unwrap(),
    };
    node.fund(&taker.address().to_string(), 500_000, "bitcoin");
    node.fund(&taker.address().to_string(), 30 * LIQ_AMOUNT, &usdt_id);
    node.mine(1);

    let world = World {
        chain,
        evm,
        genesis,
        maker_polygon: polygon,
        maker_script: Address::from_str(&liquid_address).unwrap().script_pubkey(),
        maker_address: liquid_address.clone(),
        taker,
        node,
    };

    bad_input(&mut client, &socket);
    let reserved = l2p_attacks(&world, &mut client);
    nonces(&world, &mut client);
    p2l_attacks(&world, &mut client, reserved);
    p2l_refund(&world, &mut client);
    assert!(
        world.chain.spender(&reserved).unwrap().is_none(),
        "after every payment above, the fee coin promised to a stored signature is still unspent"
    );
    println!("    ok: a reserved fee coin survives all of the wallet's payments");

    println!("\n==> Memory survives a restart");
    let terms = p2l_collected(&world, &mut client);
    signer.restart();
    let mut client = Client::connect(&socket).unwrap();
    assert_refused(client.call(&Call::RefundLiquid { terms }).unwrap(), Refusal::RefundAfterClaim);
    println!("    ok: the refund is still refused after the signer restarts");

    drop(client);
    drop(signer);
    withdrawals(&world, &config, &root, &private);

    drop(world);
    let _ = std::fs::remove_dir_all(&root);
    println!("\nALL SIGNER ATTACKS REFUSED");
}

fn bad_input(client: &mut Client, socket: &Path) {
    println!("\n==> Bad input");
    assert!(client.call_raw(b"not json\n").unwrap().contains("malformed"));
    assert!(client.call_raw(b"{\"LockLiquid\":{}}\n").unwrap().contains("malformed"));
    assert!(client.call_raw(b"\xff\xfe\n").unwrap().contains("malformed"));
    println!("    ok: malformed requests get an error");
    let mut big = Client::connect(socket).unwrap();
    let mut huge = vec![b'x'; MAX_REQUEST_BYTES as usize + 10];
    huge.push(b'\n');
    assert!(big.call_raw(&huge).unwrap().contains("too large"));
    println!("    ok: an oversized request is refused");
    let mut stalled = std::os::unix::net::UnixStream::connect(socket).unwrap();
    std::io::Write::write_all(&mut stalled, b"{\"LockLiquid\":").unwrap();
    let mut other = Client::connect(socket).unwrap();
    assert!(matches!(other.call(&Call::Identity).unwrap(), Reply::Identity { .. }), "not blocked by a stalled client");
    println!("    ok: a stalled client blocks nobody");
    let flood: Vec<_> =
        (0..liq2any_signer::MAX_CONNECTIONS * 2).map(|_| std::os::unix::net::UnixStream::connect(socket).unwrap()).collect();
    assert!(matches!(client.call(&Call::Identity).unwrap(), Reply::Identity { .. }), "an existing connection still works");
    drop(flood);
    println!("    ok: a connection flood doesn't affect an existing connection");
}

struct World {
    node: Node,
    chain: Esplora,
    evm: Evm,
    genesis: BlockHash,
    maker_polygon: String,
    maker_script: Script,
    maker_address: String,
    taker: Wallet,
}

impl World {
    fn terms(&self, direction: Direction, point: LockPoint, lock_height: u32, liquid: u64, polygon: u64) -> SwapTerms {
        SwapTerms {
            direction,
            lock_point: point,
            taker_key: LockPoint(self.taker.key.public_key().serialize()),
            blinding_key: random32().unwrap(),
            lock_height,
            liquid_amount: liquid,
            polygon_amount: polygon,
            taker_polygon: TAKER.into(),
            gas_to: None,
            gas_drop: 0,
        }
    }

    /// Pay a confidential address from the taker's wallet, and mine, so the next payment sees the change.
    fn pay(&self, address: &Address, asset: AssetId, amount: u64) {
        let payment = Payment { asset, amount, address: address.clone() };
        let tx = build_confidential_pay(&self.chain, &self.taker, vec![payment], self.genesis).unwrap();
        self.chain.broadcast(&tx).unwrap();
        self.node.mine(1);
    }

    fn top_up(&self, client: &mut Client) {
        if let Reply::Liquid(hex) = client.call(&Call::TopUpFeeCoins).unwrap() {
            let tx: LiquidTx = deserialize(&unhex(&hex).unwrap()).unwrap();
            self.chain.broadcast(&tx).unwrap();
            self.node.mine(1);
        }
    }

    /// The taker's own P→L lock, sent from its own key.
    fn taker_lock(&self, point: &LockPoint, recipient: &str, secs: u64, long_leg: bool, amount: u64) -> [u8; 32] {
        let (time, block) = self.evm.now();
        let address = lock_address(point);
        self.evm.send(
            TAKER_KEY,
            &self.evm.escrow,
            "lock(address,address,uint128,uint64,uint64,bool,address)",
            &[
                &address.to_string(),
                recipient,
                &amount.to_string(),
                &(time + secs).to_string(),
                &(block + secs).to_string(),
                &long_leg.to_string(),
                "0x0000000000000000000000000000000000000000",
            ],
        );
        self.evm.finalize();
        lock_id(&EvmAddress::parse(TAKER).unwrap(), &address)
    }

    fn maker(&self) -> EvmAddress {
        EvmAddress::parse(&self.maker_polygon).unwrap()
    }
}

fn lock_address(point: &LockPoint) -> EvmAddress {
    EvmAddress(polygon_address_of(&point.0).unwrap())
}

fn maker_key(client: &mut Client, id: [u8; 32]) -> LockPoint {
    let Reply::SwapKey { key } = client.call(&Call::SwapKey { swap_id: id }).unwrap() else { panic!() };
    key
}

fn fresh_point() -> ([u8; 32], LockPoint) {
    let secret = random32().unwrap();
    (secret, LockPoint(point_of(&secret).unwrap()))
}

/// The same x with the other y. Its swap id is identical, and only the holder of the secret could use it.
fn flipped(point: &LockPoint) -> LockPoint {
    let mut other = point.0;
    other[0] = if other[0] == 2 { 3 } else { 2 };
    LockPoint(other)
}

fn template(reply: Reply) -> (LiquidTx, Nonce) {
    let Reply::ClaimTemplate { tx, nonce } = reply else { panic!("expected a claim template, got {reply:?}") };
    (deserialize(&unhex(&tx).unwrap()).unwrap(), nonce_from(&unhex(&nonce).unwrap()).unwrap())
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn swap_coin(world: &World, terms: &SwapTerms, key: &LockPoint) -> SecretCoin {
    let address = terms.address(key).unwrap();
    world.chain.secret_coins(&address, &terms.blinding().unwrap()).unwrap().remove(0)
}

/// The taker's whole L→P side: pay, confirm, and answer the maker's claim template. Leaves an adapter signature stored.
fn l2p_ready(world: &World, client: &mut Client, terms: &SwapTerms) -> LiquidTx {
    let key = maker_key(client, terms.swap_id());
    world.pay(&terms.address(&key).unwrap(), usdt(), terms.liquid_amount);
    world.node.mine(1);
    let coin = swap_coin(world, terms, &key);
    let (tx, nonce) = template(client.call(&Call::PrepareClaim { terms: terms.clone() }).unwrap());
    let (taker_nonce, share) = answer_claim(
        &world.chain,
        &world.taker.key,
        &terms.output(&key).unwrap(),
        &coin.coin.outpoint,
        &terms.lock_point.0,
        &nonce,
        &tx,
        world.genesis,
    )
    .unwrap();
    let reply = client
        .call(&Call::StoreAdaptor {
            terms: terms.clone(),
            taker_nonce: hex_of(&nonce_bytes(&taker_nonce)),
            taker_share: hex_of(&share_bytes(&share)),
        })
        .unwrap();
    assert_eq!(reply, Reply::Held(true));
    tx
}

/// An L→P swap holding an adapter signature that hasn't been used. Its fee coin must survive anything the wallet pays.
fn reserved_fee_coin(world: &World, client: &mut Client) -> OutPoint {
    let (_, point) = fresh_point();
    let terms = world.terms(Direction::L2P, point, world.chain.tip().unwrap() + 480, LIQ_AMOUNT, USDT0);
    l2p_ready(world, client, &terms).input[1].previous_output
}

fn l2p_attacks(world: &World, client: &mut Client) -> OutPoint {
    println!("\n==> L to P: asking for a signing session");
    let tip = world.chain.tip().unwrap();
    let lock_height = tip + 480;
    let (secret, point) = fresh_point();
    let id = point.id();
    let gas_to = "0x000000000000000000000000000000000000aa02";
    let terms = SwapTerms {
        gas_to: Some(gas_to.into()),
        gas_drop: 1_000_000_000_000_000,
        ..world.terms(Direction::L2P, point, lock_height, LIQ_AMOUNT, USDT0)
    };
    let key = maker_key(client, id);
    let address = terms.address(&key).unwrap();
    let prepare = |c: &mut Client, t: &SwapTerms| c.call(&Call::PrepareClaim { terms: t.clone() }).unwrap();

    // A colluding taker who never paid can't get a session, or a fee coin reserved.
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);
    // A coin of the wrong asset at the right address. The chain has no opinion: only unblinding can tell.
    world.pay(&address, lbtc(), LIQ_AMOUNT);
    world.node.mine(1);
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);
    println!("    ok: a coin of another asset at the swap address is refused");
    // A coin of the wrong amount.
    let (_, short_point) = fresh_point();
    let short = world.terms(Direction::L2P, short_point, lock_height, LIQ_AMOUNT, USDT0);
    let short_key = maker_key(client, short_point.id());
    world.pay(&short.address(&short_key).unwrap(), usdt(), LIQ_AMOUNT - 1);
    world.node.mine(1);
    assert_refused(prepare(client, &short), Refusal::NoTakerCoin);
    println!("    ok: a coin one unit short is refused");
    // The right coin, too new, and then read with the wrong blinding key.
    world.pay(&address, usdt(), LIQ_AMOUNT);
    assert_refused(prepare(client, &terms), Refusal::NoTakerCoin);
    println!("    ok: a coin with too few confirmations is refused");
    world.node.mine(1);
    let unreadable = SwapTerms { blinding_key: random32().unwrap(), ..terms.clone() };
    assert_refused(prepare(client, &unreadable), Refusal::NoTakerCoin);
    println!("    ok: a coin that can't be read counts as no coin");
    // An explicit coin of a fee coin's value must not be taken for one.
    let plain = Address::from_str(&world.maker_address).unwrap().to_unconfidential();
    world.node.fund(&plain.to_string(), FEE, "bitcoin");
    world.node.mine(1);
    assert_failed(prepare(client, &terms), "fee coin");
    println!("    ok: no session starts without a confirmed fee coin");
    let top_up = client.call(&Call::TopUpFeeCoins).unwrap();
    assert_eq!(client.call(&Call::TopUpFeeCoins).unwrap(), top_up, "a retry sends the same bytes, and doesn't pay twice");
    world.chain.broadcast(&liquid_tx(top_up)).unwrap();
    world.node.mine(1);
    assert_eq!(client.call(&Call::TopUpFeeCoins).unwrap(), Reply::Nothing, "the pool is full");
    println!("    ok: a fee coin top-up is idempotent");

    println!("\n==> L to P: locking our funds");
    let (time, block) = world.evm.finalized();
    let (dt, db) = (time + 3 * 3_600, block + 11_000);
    let lock = |c: &mut Client, t: &SwapTerms, deadline_time, deadline_block| {
        c.call(&Call::LockPolygon { terms: t.clone(), deadline_time, deadline_block }).unwrap()
    };
    // The coin is good, but with no adapter signature we'd have no way to collect it.
    assert_refused(lock(client, &terms, dt, db), Refusal::NoAdaptor);
    println!("    ok: no lock without a stored adapter signature");

    let coins = world.chain.secret_coins(&address, &terms.blinding().unwrap()).unwrap();
    let coin = exact_secret_coin(&coins, usdt(), LIQ_AMOUNT, world.chain.tip().unwrap(), 2).unwrap();
    let output = terms.output(&key).unwrap();
    let answer = |tx: &LiquidTx, nonce: &Nonce, wrong_point: Option<[u8; 33]>| {
        let at = wrong_point.unwrap_or(terms.lock_point.0);
        answer_claim(&world.chain, &world.taker.key, &output, &coin.coin.outpoint, &at, nonce, tx, world.genesis).unwrap()
    };
    let store = |nonce: &Nonce, share| Call::StoreAdaptor {
        terms: terms.clone(),
        taker_nonce: hex_of(&nonce_bytes(nonce)),
        taker_share: hex_of(&share_bytes(&share)),
    };
    // A share locked to another point would leave the maker unable to finish, after it has locked.
    let (tx, nonce) = template(prepare(client, &terms));
    let (other_nonce, other_share) = answer(&tx, &nonce, Some(point_of(&random32().unwrap()).unwrap()));
    assert!(matches!(client.call(&store(&other_nonce, other_share)).unwrap(), Reply::Failed(_)));
    assert_eq!(client.call(&Call::AdaptorHeld { swap_id: id }).unwrap(), Reply::Held(false));
    assert_refused(lock(client, &terms, dt, db), Refusal::NoAdaptor);
    println!("    ok: a share for another point is refused, and nothing is stored");
    assert_failed(client.call(&store(&other_nonce, other_share)).unwrap(), "session");
    println!("    ok: a used session can't be used again");
    let _ = template(prepare(client, &terms));
    let junk = Call::StoreAdaptor { terms: terms.clone(), taker_nonce: "00".repeat(66), taker_share: "ff".repeat(32) };
    assert!(matches!(client.call(&junk).unwrap(), Reply::Failed(_)));
    println!("    ok: malformed nonces and shares are refused");
    let (tx, nonce) = template(prepare(client, &terms));
    let fee_outpoint = tx.input[1].previous_output;
    let (good_nonce, good_share) = answer(&tx, &nonce, None);
    assert_eq!(client.call(&store(&good_nonce, good_share)).unwrap(), Reply::Held(true));
    assert_eq!(prepare(client, &terms), Reply::Nothing, "no second session once one is stored");
    assert!(matches!(client.call(&store(&good_nonce, good_share)).unwrap(), Reply::Refused(_)));
    println!("    ok: a real share is checked and stored, once");

    // The engine lies that the coin covers more than it does, at a loss.
    let (_, cheap_point) = fresh_point();
    let cheap = world.terms(Direction::L2P, cheap_point, lock_height, LIQ_AMOUNT - 100, USDT0);
    l2p_ready(world, client, &cheap);
    assert_refused(lock(client, &cheap, dt, db), Refusal::BadPrice);
    // Bad timing, on either clock, and a measured clock the signer reads itself.
    assert_bad_timeout(lock(client, &terms, time + 3 * 3_600 + 600, db));
    assert_bad_timeout(lock(client, &terms, dt, block + 20_000));
    assert_bad_timeout(lock(client, &terms, time, db));
    println!("    ok: deadlines past either clock's limit are refused");
    // A gas drop over the cap.
    let greedy = SwapTerms { gas_drop: 10_000_000_000_000_000_000, ..terms.clone() };
    assert_refused(lock(client, &greedy, dt, db), Refusal::GasDropTooLarge);
    // A taker Tether has blocked would be paid, and stuck.
    world.evm.block(TAKER, true);
    assert_refused(lock(client, &terms, dt, db), Refusal::Blocked);
    world.evm.block(TAKER, false);
    // Not approved yet: nothing is signed, so no nonce is taken.
    assert_failed(lock(client, &terms, dt, db), "ApproveEscrow");
    let approve = client.call(&Call::ApproveEscrow).unwrap();
    let approve_nonce = world.evm.publish(&approve);
    println!("    ok: the escrow was approved, at nonce {approve_nonce}");

    // Now everything is right. The signer signs, and doesn't send.
    let first = lock(client, &terms, dt, db);
    let Reply::Polygon { nonce: lock_nonce, .. } = &first else { panic!("{first:?}") };
    let ours = lock_id(&world.maker(), &lock_address(&point));
    assert_eq!(world.evm.lock(&ours).state, LockState::None, "the signer never broadcasts");
    println!("    ok: signed at nonce {lock_nonce}, and not sent");
    // An identical retry gets the same nonce and bytes. Different terms get nothing.
    assert_eq!(lock(client, &terms, dt, db), first, "a retry is the same transaction");
    assert_refused(lock(client, &terms, dt - 1, db), Refusal::AlreadyLocked);
    assert_refused(lock(client, &SwapTerms { gas_drop: 1, ..terms.clone() }, dt, db), Refusal::AlreadyLocked);
    println!("    ok: a retry is byte-identical, and different terms are refused");
    let raw = decode_signed(&from_hex(match &first { Reply::Polygon { raw, .. } => raw, _ => unreachable!() }).unwrap()).unwrap();
    assert_eq!((raw.to, raw.value), (world.evm.escrow, 1_000_000_000_000_000), "to the escrow, carrying the gas drop");

    world.evm.publish(&first);
    let l = world.evm.lock(&ours);
    assert_eq!((l.state, l.long_leg, l.amount, l.deadline_time, l.deadline_block), (LockState::Open, false, u128::from(USDT0), dt, db));
    assert_eq!(l.recipient, EvmAddress::parse(TAKER).unwrap());
    assert_eq!(world.evm.rpc.balance(&EvmAddress::parse(gas_to).unwrap(), "latest").unwrap(), 1_000_000_000_000_000);
    println!("    ok: the real lock is a short leg to the taker, and the gas drop arrived");
    // Our refund would fail while the taker may still claim: the signer checks before taking a nonce.
    assert_failed(client.call(&Call::RefundPolygon { lock_point: point }).unwrap(), "would fail");
    println!("    ok: no refund is signed while the claim is open");

    // The claim only takes the right secret, and always pays our own wallet.
    assert_refused(client.call(&Call::ClaimLiquid { terms: terms.clone(), secret: [9; 32] }).unwrap(), Refusal::WrongSecret);
    let s = polygon_value(&secret).unwrap();
    world.evm.send(TAKER_KEY, &world.evm.escrow, "claim(bytes32,bytes32)", &[&format!("0x{}", hex_of(&ours)), &format!("0x{}", hex_of(&s))]);
    let learned = secret_from_polygon(&world.evm.lock(&ours).revealed).unwrap();
    assert_eq!(learned, secret, "the secret comes out of what the claim revealed");
    let claim = liquid_tx(client.call(&Call::ClaimLiquid { terms: terms.clone(), secret: learned }).unwrap());
    assert_eq!(claim.output[0].script_pubkey, world.maker_script, "the claim pays our wallet");
    assert!(!claim.output[0].asset.is_explicit() && !claim.output[0].value.is_explicit(), "and hides what it pays");
    assert_eq!(claim.input[1].previous_output, fee_outpoint, "using the fee coin it reserved");
    world.chain.broadcast(&claim).unwrap();
    world.node.mine(1);
    let mined = world.chain.tx(&claim.txid()).unwrap();
    assert_eq!(is_key_path_spend(&mined, &coin.coin.outpoint), Some(true), "one signature on-chain, with no script");
    println!("    ok: the claim paid our wallet, confirmed, and looks like a plain key-path spend");

    // Requests in the wrong direction for this swap.
    assert_refused(
        client.call(&Call::ClaimPolygon { taker_polygon: TAKER.into(), lock_point: point, secret }).unwrap(),
        Refusal::UnknownSwap,
    );
    assert_refused(
        client.call(&Call::Release { taker_polygon: TAKER.into(), lock_point: point }).unwrap(),
        Refusal::ReleaseAfterLock,
    );
    world.top_up(client);
    reserved_fee_coin(world, client)
}

/// One call per nonce: a pending transaction is re-signed at a higher fee when the network's rises, at the same nonce, and
/// nothing else ever takes that nonce.
fn nonces(world: &World, client: &mut Client) {
    println!("\n==> Polygon nonces");
    // The escrow's approval was mined earlier: asking again gives the mined transaction back, and signs nothing new.
    let Reply::Polygon { nonce: approved, .. } = client.call(&Call::ApproveEscrow).unwrap() else { panic!() };
    assert_eq!(approved, 1, "the nonce it was mined at");
    // A lock signed and not sent.
    let (_, point) = fresh_point();
    let terms = world.terms(Direction::L2P, point, world.chain.tip().unwrap() + 480, LIQ_AMOUNT, USDT0);
    l2p_ready(world, client, &terms);
    let (time, block) = world.evm.finalized();
    let lock = Call::LockPolygon { terms: terms.clone(), deadline_time: time + 3 * 3_600, deadline_block: block + 11_000 };
    let first = client.call(&lock).unwrap();
    let Reply::Polygon { nonce, raw, .. } = &first else { panic!("{first:?}") };
    assert_eq!(client.call(&lock).unwrap(), first, "unmined, and fees unchanged: the same transaction");
    // The network's base fee jumps before it is mined. Pending transactions come back re-signed, outbidding the old fee.
    let before = decode_signed(&from_hex(raw).unwrap()).unwrap();
    let url = &world.evm.url;
    cast(&["rpc", "anvil_setNextBlockBaseFeePerGas", &format!("0x{:x}", before.max_fee_per_gas * 3), "--rpc-url", url]);
    cast(&["rpc", "evm_mine", "--rpc-url", url]);
    let Reply::PolygonPending(pending) = client.call(&Call::PendingPolygon).unwrap() else { panic!() };
    assert_eq!(pending.len(), 1, "only the unmined one");
    let after = decode_signed(&from_hex(&pending[0]).unwrap()).unwrap();
    assert_eq!((after.nonce, after.to, &after.data, after.value), (*nonce, before.to, &before.data, before.value), "the same call at the same nonce");
    assert!(after.max_fee_per_gas > before.max_fee_per_gas * 9 / 8, "outbidding by more than 12.5%");
    assert!(after.max_priority_fee_per_gas > before.max_priority_fee_per_gas, "on the priority fee too");
    // Asking for the lock again gives the new bytes, at the same nonce.
    let Reply::Polygon { raw: again, nonce: same, .. } = client.call(&lock).unwrap() else { panic!() };
    assert_eq!((same, again.clone()), (*nonce, pending[0].clone()));
    world.evm.rpc.send_raw(&from_hex(&pending[0]).unwrap()).unwrap();
    let ours = lock_id(&world.maker(), &lock_address(&point));
    let start = Instant::now();
    while world.evm.lock(&ours).state != LockState::Open {
        assert!(start.elapsed() < Duration::from_secs(20), "the re-signed lock didn't land");
        std::thread::sleep(Duration::from_millis(100));
    }
    let Reply::PolygonPending(pending) = client.call(&Call::PendingPolygon).unwrap() else { panic!() };
    assert!(pending.is_empty(), "mined, so nothing pending");
    println!("    ok: one call per nonce, re-signed at a higher fee while pending, landed, and never replaced by another");
}

fn p2l_attacks(world: &World, client: &mut Client, reserved: OutPoint) {
    println!("\n==> P to L: locking our funds");
    let maker = world.maker_polygon.clone();
    let lock_height = || world.chain.tip().unwrap() + 80;
    let terms_for = |point, height| world.terms(Direction::P2L, point, height, LIQ_AMOUNT, USDT0);
    let lock_call = |t: &SwapTerms| Call::LockLiquid { terms: t.clone() };
    let release = |c: &mut Client, lock_point| {
        let reply = c.call(&Call::Release { taker_polygon: TAKER.into(), lock_point }).unwrap();
        world.evm.publish(&reply);
    };
    let eight_hours = 8 * 3_600;

    // No lock at all.
    let (_, point) = fresh_point();
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    // A lock paying someone else.
    let (_, point) = fresh_point();
    world.taker_lock(&point, STRANGER, eight_hours, true, USDT0);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    // Too close to its deadlines, which we can return at once with Release.
    let (_, point) = fresh_point();
    let near = world.taker_lock(&point, &maker, 3_600, true, USDT0);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    release(client, point);
    assert_eq!(world.evm.lock(&near).state, LockState::Refunded);
    println!("    ok: Release returned an unfilled lock");
    // A short leg, whose claim closes on either clock, could strand us after a reveal.
    let (_, point) = fresh_point();
    let short = world.taker_lock(&point, &maker, eight_hours, false, USDT0);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::NoTakerEscrow);
    release(client, point);
    assert_eq!(world.evm.lock(&short).state, LockState::Refunded);
    // Paying out more than the lock holds.
    let (_, point) = fresh_point();
    let under = world.taker_lock(&point, &maker, eight_hours, true, USDT0 - 1);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::BadPrice);
    release(client, point);
    assert_eq!(world.evm.lock(&under).state, LockState::Refunded);
    // We are blocked, and couldn't move what we'd claim.
    let (_, point) = fresh_point();
    world.taker_lock(&point, &maker, eight_hours, true, USDT0);
    world.evm.block(&maker, true);
    assert_refused(client.call(&lock_call(&terms_for(point, lock_height()))).unwrap(), Refusal::Blocked);
    world.evm.block(&maker, false);
    release(client, point);

    // A good one, with the double-payment guard.
    let (secret, point) = fresh_point();
    let id = point.id();
    let taker_id = world.taker_lock(&point, &maker, eight_hours, true, USDT0);
    let height = lock_height();
    let terms = terms_for(point, height);
    let key = maker_key(client, id);
    let early = Call::OpenSession { terms: terms.clone(), claim_tx: "00".into(), taker_nonce: "00".into() };
    assert_refused(client.call(&early).unwrap(), Refusal::UnknownSwap);
    println!("    ok: no signing session before we've locked");
    let first = client.call(&lock_call(&terms)).unwrap();
    assert_eq!(client.call(&lock_call(&terms)).unwrap(), first, "a retry gets the exact same bytes");
    assert_refused(client.call(&lock_call(&SwapTerms { lock_height: height + 1, ..terms.clone() })).unwrap(), Refusal::AlreadyLocked);
    println!("    ok: a retried Liquid lock is byte-identical");
    let lock = liquid_tx(first);
    assert!(lock.output.iter().filter(|o| !o.is_fee()).all(|o| !o.asset.is_explicit() && !o.value.is_explicit()));
    assert!(lock.input.iter().all(|i| i.previous_output != reserved), "a coin promised to a signature stays out");
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);
    let coin = swap_coin(world, &terms, &key);
    assert_eq!((coin.secrets.asset, coin.secrets.value), (usdt(), LIQ_AMOUNT));
    let fee_address = terms.taker_fee_address().unwrap();
    let fee_coin = world.chain.secret_coins(&fee_address, &terms.blinding().unwrap()).unwrap().remove(0);
    assert_eq!((fee_coin.secrets.asset, fee_coin.secrets.value), (lbtc(), FEE));
    println!("    ok: the taker can read its coin and its fee coin, with exact amounts");

    coins_are_never_reused(world, client);
    assert_bad_timeout(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    assert_refused(client.call(&Call::Release { taker_polygon: TAKER.into(), lock_point: point }).unwrap(), Refusal::ReleaseAfterLock);

    println!("\n==> P to L: the signing session");
    let output = terms.output(&key).unwrap();
    let to = world.taker.address();
    let open = |c: &mut Client, tx: &LiquidTx, nonce: &Nonce| {
        c.call(&Call::OpenSession { terms: terms.clone(), claim_tx: serialize_hex(tx), taker_nonce: hex_of(&nonce_bytes(nonce)) })
            .unwrap()
    };
    let start = || {
        let (tx, _) = build_swap_spend(&coin, &fee_coin, &to, 0, Sequence::MAX).unwrap();
        TakerClaim::start(&world.chain, &world.taker.key, &output, tx, world.genesis).unwrap()
    };
    let got_nonce = |reply: Reply| {
        let Reply::Nonce(hex) = reply else { panic!("expected a nonce, got {reply:?}") };
        nonce_from(&unhex(&hex).unwrap()).unwrap()
    };
    let (mut wrong, _) = build_swap_spend(&coin, &fee_coin, &to, 0, Sequence::MAX).unwrap();
    wrong.input.swap(0, 1);
    assert_failed(open(client, &wrong, &start().nonce()), "input 0");
    println!("    ok: a claim that doesn't spend the swap coin first is refused");
    let finish = |c: &mut Client, t: &SwapTerms, share: &str| {
        c.call(&Call::FinishSession { terms: t.clone(), taker_share: share.to_string() }).unwrap()
    };
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (bad, _) = claim.sign(&world.taker.key, &output, &point_of(&random32().unwrap()).unwrap(), &maker_nonce).unwrap();
    assert!(matches!(finish(client, &terms, &hex_of(&share_bytes(&bad))), Reply::Failed(_)));
    assert_failed(finish(client, &terms, &hex_of(&share_bytes(&bad))), "session");
    println!("    ok: a share for another point gets no share back, and that session is gone");
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (share, _) = claim.sign(&world.taker.key, &output, &point.0, &maker_nonce).unwrap();
    let moved = SwapTerms { lock_height: terms.lock_height + 1, ..terms.clone() };
    assert_failed(finish(client, &moved, &hex_of(&share_bytes(&share))), "differ");
    println!("    ok: a session can't be finished under different terms");
    // The same x with the other y: same id, but its Polygon address isn't the lock's.
    let claim = start();
    let maker_nonce = got_nonce(open(client, &claim.tx.clone(), &claim.nonce()));
    let (share, signed) = claim.sign(&world.taker.key, &output, &point.0, &maker_nonce).unwrap();
    let flip = SwapTerms { lock_point: flipped(&point), ..terms.clone() };
    assert_refused(finish(client, &flip, &hex_of(&share_bytes(&share))), Refusal::NoTakerEscrow);
    println!("    ok: a point with the same x and the other y is refused");
    let Reply::Share(given) = finish(client, &terms, &hex_of(&share_bytes(&share))) else { panic!() };
    assert_eq!(finish(client, &terms, &hex_of(&share_bytes(&share))), Reply::Share(given.clone()), "resent unchanged");
    let again = start();
    assert_refused(open(client, &again.tx.clone(), &again.nonce()), Refusal::AdaptorAlreadyHeld);
    println!("    ok: our share is given once, resent unchanged, and there is no second session");
    let maker_share = share_from(&unhex(&given).unwrap()).unwrap();
    let claim_tx = signed.finish(&world.taker.key, &output, &key.0, maker_share, &point.0, &secret, world.genesis).unwrap();
    let published = published_signature(&claim_tx, &coin.coin.outpoint).unwrap();

    // Extend: the tip is fresh, so refused, then Liquid stalls, so allowed, within the limits.
    let l = world.evm.lock(&taker_id);
    let extend = |c: &mut Client, new_time, new_block| {
        c.call(&Call::Extend { taker_polygon: TAKER.into(), lock_point: point, new_time, new_block }).unwrap()
    };
    world.node.mine(1);
    assert_refused(extend(client, l.deadline_time + 1_000, l.deadline_block + 1_000), Refusal::ExtendNotJustified);
    std::thread::sleep(Duration::from_secs(5));
    assert_bad_timeout(extend(client, l.deadline_time + 7 * 86_400 + 1, l.deadline_block + 1_000));
    assert_bad_timeout(extend(client, l.deadline_time + 1_000, l.deadline_block + 403_201));
    let reply = extend(client, l.deadline_time + 1_000, l.deadline_block + 1_000);
    world.evm.publish(&reply);
    let moved = world.evm.lock(&taker_id);
    assert_eq!((moved.deadline_time, moved.deadline_block, moved.extended), (l.deadline_time + 1_000, l.deadline_block + 1_000, true));
    assert_failed(extend(client, moved.deadline_time + 1, moved.deadline_block + 1), "would fail");
    println!("    ok: Extend only works during a real stall, landed on both clocks, and only once");

    // The taker claims on Liquid, and we read the secret out of its signature.
    world.chain.broadcast(&claim_tx).unwrap();
    world.node.mine(1);
    let extract = |c: &mut Client, sig: &[u8]| c.call(&Call::ExtractSecret { lock_point: point, signature: hex_of(sig) }).unwrap();
    assert_eq!(extract(client, &published), Reply::Secret(secret));
    assert!(matches!(extract(client, &[7; 64]), Reply::Failed(_)), "a made-up signature gives nothing");
    println!("    ok: the secret comes out of the published signature");

    assert_refused(
        client.call(&Call::ClaimPolygon { taker_polygon: TAKER.into(), lock_point: point, secret: [9; 32] }).unwrap(),
        Refusal::WrongSecret,
    );
    let before = world.evm.token_balance(&maker);
    let collect = client.call(&Call::ClaimPolygon { taker_polygon: TAKER.into(), lock_point: point, secret }).unwrap();
    world.evm.publish(&collect);
    assert_eq!(world.evm.lock(&taker_id).state, LockState::Claimed);
    assert_eq!(world.evm.token_balance(&maker), before + u128::from(USDT0));
    println!("    ok: collected on Polygon with the secret from Liquid");
}

/// Two locks with no block between them. The node's coin list still shows the first one's coins as unspent, and a second lock
/// signed over them could never land, because retries must send the same bytes.
fn coins_are_never_reused(world: &World, client: &mut Client) {
    let maker = world.maker_polygon.clone();
    let mut seen: Vec<OutPoint> = vec![];
    for _ in 0..2 {
        let (_, point) = fresh_point();
        world.taker_lock(&point, &maker, 8 * 3_600, true, USDT0);
        let height = world.chain.tip().unwrap() + 80;
        let call = Call::LockLiquid { terms: world.terms(Direction::P2L, point, height, LIQ_AMOUNT, USDT0) };
        let reply = match client.call(&call).unwrap() {
            Reply::Failed(e) if e.contains("wallet") => {
                world.node.mine(1);
                client.call(&call).unwrap()
            }
            other => other,
        };
        let tx = liquid_tx(reply);
        for input in &tx.input {
            assert!(!seen.contains(&input.previous_output), "a signed payment's coin was used again");
            seen.push(input.previous_output);
        }
        world.chain.broadcast(&tx).expect("each lock can land");
    }
    world.node.mine(1);
    println!("    ok: locks signed in one block never share a coin, and both land");
}

/// A taker who never claims: the maker's own coin comes back through the Simplicity leaf.
fn p2l_refund(world: &World, client: &mut Client) {
    println!("\n==> P to L: refund");
    world.top_up(client);
    let (_, point) = fresh_point();
    world.taker_lock(&point, &world.maker_polygon, 8 * 3_600, true, USDT0);
    let height = world.chain.tip().unwrap() + 30;
    let terms = world.terms(Direction::P2L, point, height, LIQ_AMOUNT, USDT0);
    let key = maker_key(client, point.id());
    let lock = liquid_tx(client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap());
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);
    let coin = swap_coin(world, &terms, &key);
    assert_bad_timeout(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    world.node.mine(30);
    let refund = liquid_tx(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap());
    assert_eq!(refund.output[0].script_pubkey, world.maker_script, "the refund pays our wallet");
    world.chain.broadcast(&refund).unwrap();
    world.node.mine(1);
    let mined = world.chain.tx(&refund.txid()).unwrap();
    assert_eq!(is_key_path_spend(&mined, &coin.coin.outpoint), Some(false), "a refund is recognisably not a claim");
    println!("    ok: the refund went through the Simplicity leaf and paid our wallet");
}

/// Collect on Polygon, then try to take the Liquid coin back too.
fn p2l_collected(world: &World, client: &mut Client) -> SwapTerms {
    let (secret, point) = fresh_point();
    let taker_id = world.taker_lock(&point, &world.maker_polygon, 8 * 3_600, true, USDT0);
    let height = world.chain.tip().unwrap() + 40;
    let terms = world.terms(Direction::P2L, point, height, LIQ_AMOUNT, USDT0);
    let lock = liquid_tx(client.call(&Call::LockLiquid { terms: terms.clone() }).unwrap());
    world.chain.broadcast(&lock).unwrap();
    world.node.mine(2);
    let claim = client.call(&Call::ClaimPolygon { taker_polygon: TAKER.into(), lock_point: point, secret }).unwrap();
    world.evm.publish(&claim);
    assert_eq!(world.evm.lock(&taker_id).state, LockState::Claimed);
    world.node.mine(45);
    assert!(world.chain.tip().unwrap() >= height, "past the lock height");
    assert_refused(client.call(&Call::RefundLiquid { terms: terms.clone() }).unwrap(), Refusal::RefundAfterClaim);
    println!("    ok: after collecting on Polygon, the refund is refused, even past the lock height");
    terms
}

fn withdrawals(world: &World, config: &Config, root: &Path, private: &Path) {
    println!("\n==> Withdrawals go only where the config says");
    let start = |name: &str, to: WithdrawTo| {
        let cfg = Config {
            socket: private.join(format!("{name}.sock")),
            ledger: root.join(format!("{name}.db")),
            withdraw_to: to,
            ..config.clone()
        };
        let path = root.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_string(&cfg).unwrap()).unwrap();
        (path, cfg.socket)
    };
    let (path, socket) = start("w-none", WithdrawTo::default());
    let none = SignerProcess::start(&path, &socket);
    let mut client = Client::connect(&socket).unwrap();
    for chain in [Chain::Liquid, Chain::Polygon] {
        assert_failed(client.call(&Call::Withdraw { chain, asset: Asset::Usdt, amount: None }).unwrap(), "withdrawal address");
    }
    drop(client);
    drop(none);
    println!("    ok: without withdraw_to, nothing can be withdrawn");

    // A new ledger, at the same key: the nonces it signs must still follow the chain's.
    let cold = "0x000000000000000000000000000000000000c01d";
    let (path, socket) = start("w", WithdrawTo { liquid: None, polygon: Some(cold.into()) });
    let _signer = SignerProcess::start(&path, &socket);
    let mut client = Client::connect(&socket).unwrap();
    let held = world.evm.token_balance(&world.maker_polygon);
    let some = client.call(&Call::Withdraw { chain: Chain::Polygon, asset: Asset::Usdt, amount: Some(u128::from(USDT0)) }).unwrap();
    world.evm.publish(&some);
    assert_eq!(world.evm.token_balance(cold), u128::from(USDT0));
    // A caller naming its own destination: the field isn't part of the request, so it changes nothing.
    let sneaky = format!(r#"{{"Withdraw":{{"chain":"Polygon","asset":"Usdt","amount":null,"to":"{STRANGER}"}}}}"#);
    let line = client.call_raw(format!("{sneaky}\n").as_bytes()).unwrap();
    world.evm.publish(&serde_json::from_str(&line).unwrap());
    assert_eq!(world.evm.token_balance(cold), held, "all of it reached the configured address");
    assert_eq!(world.evm.token_balance(&world.maker_polygon), 0);
    println!("    ok: USDT0, some then all, went to the configured address, though a request named another");
    let pol = client.call(&Call::Withdraw { chain: Chain::Polygon, asset: Asset::Gas, amount: None }).unwrap();
    world.evm.publish(&pol);
    let left = world.evm.rpc.balance(&world.maker(), "latest").unwrap();
    assert!(left < 10_000_000_000_000_000, "nearly all the POL went: {left} wei left");
    assert!(world.evm.rpc.balance(&EvmAddress::parse(cold).unwrap(), "latest").unwrap() > 9_000 * 1_000_000_000_000_000_000);
    println!("    ok: and the POL, less this transfer's gas");
}
