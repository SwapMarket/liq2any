// What this page talks to. Only test networks exist until the escrow is deployed on Polygon mainnet (decision P3). Endpoints and
// relays can be changed in Settings. The escrow, token, chain, assets and genesis are pinned here per network, and the swap logic
// holds the maker to them.
const TESTNET = {
  name: 'testnet',
  label: 'Liquid testnet + Polygon Amoy',
  genesis: 'a771da8e52ee6ad581ed1e9a99825e5b3b7992225534eaa2ae23244fe26ab1c1',
  usdt: 'b612eb46313a2cd6ebabd8b7a8eed5696e29898b87a43bff41c94f51acef9d73',
  lbtc: '144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49',
  regtest: false,
  // How a confidential address starts here, for the examples in the address fields.
  addressHint: 'tlq1…',
  esplora: ['https://blockstream.info/liquidtestnet/api', 'https://liquid.network/liquidtestnet/api'],
  explorer: 'https://blockstream.info/liquidtestnet',
  // Two independent endpoints, both with open CORS headers (docs/design.md section 6.6). A claim waits until both agree.
  polygon: ['https://polygon-amoy-bor-rpc.publicnode.com', 'https://polygon-amoy.drpc.org'],
  polygonExplorer: (kind, id) => `https://amoy.polygonscan.com/${kind}/${id}`,
  chainId: 80002,
  // What a wallet is asked to add, if it doesn't know the chain.
  chainParams: {
    chainId: '0x13882',
    chainName: 'Polygon Amoy',
    nativeCurrency: { name: 'POL', symbol: 'POL', decimals: 18 },
    rpcUrls: ['https://polygon-amoy-bor-rpc.publicnode.com'],
    blockExplorerUrls: ['https://amoy.polygonscan.com'],
  },
  walletConnectChain: 'eip155:80002',
  // contracts/polygon/deployments/amoy.json: the escrow, and the stand-in for USDT0, which has no testnet.
  escrow: '0xD1C669097D4EDb41B5A5b3541aFB5c22b7Bd6f6F',
  token: '0xb4236fbd3bc5791Ee0faB7f065f96F591d33a724',
  tokenName: 'tUSDT0',
  relays: ['wss://relay.primal.net', 'wss://relay.coinos.pro'],
  // Reown's project id is public by nature, because a static page can't hide it. It is shared with sol2base and eth2base.
  walletConnectProject: 'c001d9e3d0c6308a5ce7e5932d40064a',
  // Only on test networks, so a mainnet build has no Faucet tab. The stand-in token's own faucet gives up to 1,000 a call.
  faucet: {
    drip: '100',
    pol: 'https://faucet.polygon.technology',
    liquid: 'https://liquidtestnet.com/faucet',
  },
  pollMs: 15000,
  // Liquid confirmations a swap's coin needs before the page acts on it: `liquidConfirmations`, or `liquidConfirmationsLarge` for a
  // swap above `largeAboveUnits` (USDT units of 10^-6; null means one rule for every size). See confirmationsFor.
  liquidConfirmations: 2,
  liquidConfirmationsLarge: 2,
  largeAboveUnits: null,
  // docs/design.md section 3.4. L→P: the taker's Liquid lock runs about 8 h, and the maker's Polygon lock about 3 h, longer than
  // one span of Polygon's block producer. P→L: the maker may ask the taker's lock to leave up to 7 h when it checks it.
  rules: {
    l2p_min_blocks: 420, l2p_max_blocks: 540, p2l_min_blocks: 50, p2l_max_blocks: 150,
    l2p_min_window_secs: 10000, l2p_max_window_secs: 14400, p2l_max_window_secs: 25200,
  },
  // Don't reveal the secret with less than this left on either of the maker's Polygon clocks.
  claimCutoffSecs: 1800,
  claimCutoffBlocks: 10,
  // P→L: the taker's lock runs this much longer than the maker asks it to leave, for the time to lock and be checked.
  p2lLockMarginSecs: 5400,
  maxSpreadBps: 200,
  // A wallet needs POL for the network fee: about this much covers a lock and its approval, or a claim.
  minPolWei: 20000000000000000n,
  // A refund's fee coin comes from the user's own wallet, and any amount from this up works, because all of it pays the fee.
  // A refund needs 29 sats at the relay floor. Wallets have their own minimums: Aqua's is 100.
  refundFeeCoinSats: 30,
  // No limit of the page's own on test networks.
  maxSwapUnits: null,
};

// Real funds. None until the user deploys the escrow on Polygon mainnet, so this build offers test networks only.
const MAINNET = {
  ...TESTNET,
  name: 'mainnet',
  label: 'Liquid + Polygon mainnet',
  // blockstream.info/liquid/api/block-height/0
  genesis: '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003',
  // Tether's USDt, which the Liquid asset registry ties to tether.to, and L-BTC, the policy asset.
  usdt: 'ce091c998b83c78bb71a632313ba3760f1763d9cfcffae02258ffa9865a37bd2',
  lbtc: '6f0279e9ed041c3d710a9f57d0c02928416460c4b722ae3457a11eec381c526d',
  addressHint: 'lq1…',
  esplora: ['https://blockstream.info/liquid/api', 'https://liquid.network/api'],
  explorer: 'https://blockstream.info/liquid',
  polygon: ['https://polygon-bor-rpc.publicnode.com', 'https://polygon.drpc.org'],
  polygonExplorer: (kind, id) => `https://polygonscan.com/${kind}/${id}`,
  chainId: 137,
  chainParams: {
    chainId: '0x89',
    chainName: 'Polygon',
    nativeCurrency: { name: 'POL', symbol: 'POL', decimals: 18 },
    rpcUrls: ['https://polygon-bor-rpc.publicnode.com'],
    blockExplorerUrls: ['https://polygonscan.com'],
  },
  walletConnectChain: 'eip155:137',
  escrow: null,
  // Tether's USDT0 on Polygon PoS, 6 decimals (docs/assumptions.md section 1.1).
  token: '0xc2132D05D31c914a87C6611C10748AEb04B58e8F',
  tokenName: 'USDT0',
  faucet: null,
  // The most one swap may move, in USDT units of 10^-6, as liq2sol's 1000 USDT (decision P4).
  maxSwapUnits: 1000000000n,
  liquidConfirmations: 1,
  liquidConfirmationsLarge: 2,
  largeAboveUnits: 100000000n,
};

export const NETWORKS = { testnet: TESTNET, mainnet: MAINNET };

const CHOICE = 'liq2any.network';

export const mainnetAvailable = () => !!MAINNET.escrow;

function chosen() {
  if (!mainnetAvailable()) return 'testnet';
  try {
    if (localStorage.getItem(CHOICE) === 'testnet') return 'testnet';
  } catch {
    // No storage: nothing to keep, so the default.
  }
  return 'mainnet';
}

export const NETWORK = NETWORKS[chosen()];

// The Liquid confirmations a swap's coin needs: more for a swap above `largeAboveUnits`, the size the agreed terms say it is.
export function confirmationsFor(terms, net = NETWORK) {
  const large = net.largeAboveUnits !== null && terms && BigInt(terms.polygon_amount) > net.largeAboveUnits;
  return large ? net.liquidConfirmationsLarge : net.liquidConfirmations;
}

// Switching reloads the page, so nothing built for one network is ever used on the other.
export function chooseNetwork(name) {
  if (!NETWORKS[name] || (name === 'mainnet' && !mainnetAvailable())) throw new Error('That network is not available.');
  localStorage.setItem(CHOICE, name);
  location.reload();
}

// Each network keeps its own swaps, settings and logs.
export const storageKey = (base) => (NETWORK.name === 'mainnet' ? `${base}.mainnet` : base);

const POLYGON_MAINNET_CHAIN_ID = 137;
const LIQUID_MAINNET_GENESIS = '1466275836220db2944ca059a3a10ef6fd2ea684b0688d2c379296888a206003';

// Hold the page to its network, whatever the endpoints in Settings say: mainnet means both chains are mainnet, and a test
// network means neither is. `network` is the configuration in use, and `chainId` what the Polygon endpoint reports.
export function checkGenesis(network, liquidGenesis, chainId) {
  const main = [liquidGenesis === LIQUID_MAINNET_GENESIS, chainId === POLYGON_MAINNET_CHAIN_ID];
  if (network.name === 'mainnet') {
    if (!main[0] || !main[1]) throw new Error('Refusing to run: this is set to mainnet, and an endpoint is on another network.');
    if (!network.escrow) throw new Error('Refusing to run: the escrow is not on mainnet yet.');
  } else {
    if (main[0] || main[1]) throw new Error('Refusing to run: an endpoint is on mainnet, and this is set to a test network. Switch in Settings.');
    if (!network.regtest && chainId !== network.chainId) throw new Error(`Refusing to run: the Polygon endpoint is on chain ${chainId}, not ${network.chainId}.`);
  }
}
