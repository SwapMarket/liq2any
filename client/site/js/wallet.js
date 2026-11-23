// The Polygon wallet: extensions announced through EIP-6963 (or the one at window.ethereum), or a phone through WalletConnect. The
// page holds no Polygon key. It asks the wallet to send a call it built, `{to, data}`, after checking it would succeed.
import { NETWORK } from './config.js';

const found = new Map();

// EIP-6963: each extension announces itself with a name and its own provider, so several can be installed without fighting over
// window.ethereum. Wallets answer either event, whichever side loads first.
window.addEventListener('eip6963:announceProvider', (e) => {
  const { info, provider } = e.detail ?? {};
  if (info?.uuid && provider?.request) found.set(info.uuid, { name: info.name, icon: info.icon, provider });
});
window.dispatchEvent(new Event('eip6963:requestProvider'));

export function extensionWallets() {
  const list = [...found.values()];
  // An older extension that only sets window.ethereum.
  if (!list.length && window.ethereum?.request) list.push({ name: window.ethereum.isMetaMask ? 'MetaMask' : 'Browser wallet', provider: window.ethereum });
  return list;
}

const hexChain = () => `0x${NETWORK.chainId.toString(16)}`;

// Asks the wallet to use this page's chain, adding it if the wallet doesn't know it (EIP-3085, EIP-3326).
async function onChain(provider) {
  if ((await provider.request({ method: 'eth_chainId' })) === hexChain()) return;
  try {
    await provider.request({ method: 'wallet_switchEthereumChain', params: [{ chainId: hexChain() }] });
  } catch (e) {
    if (e?.code !== 4902 && !/unrecognized|not added|unknown chain/i.test(e?.message ?? '')) throw e;
    await provider.request({ method: 'wallet_addEthereumChain', params: [NETWORK.chainParams] });
  }
  if ((await provider.request({ method: 'eth_chainId' })) !== hexChain()) throw new Error(`Switch the wallet to ${NETWORK.chainParams.chainName}.`);
}

// A wallet the page can use: its name, its address, and `send`, which returns the transaction's hash.
function wrap(name, address, provider) {
  return {
    name,
    address,
    send: async ({ to, data, value = '0x0' }) => {
      await onChain(provider);
      return provider.request({ method: 'eth_sendTransaction', params: [{ from: address, to, data, value }] });
    },
  };
}

// silent reconnects only if the wallet already trusts this page, with no prompt.
export async function connectExtension(w, silent = false) {
  const accounts = await w.provider.request({ method: silent ? 'eth_accounts' : 'eth_requestAccounts' });
  const address = accounts?.[0];
  if (!address) throw new Error(`${w.name} gave no account.`);
  if (!silent) await onChain(w.provider);
  return wrap(w.name, address, w.provider);
}

// Loaded only when someone picks WalletConnect, or used it last time, because it is large and needs Reown's relay.
// One client for the page, per project. Each SignClient.init starts its own WalletConnect core on the same storage and relay, so with
// two (one restoring the last session at load, one connecting) the wallet's answer could reach the one that hadn't asked. It dropped
// it ("No matching key. proposal"), and the page never saw the wallet connect. The promise is kept, so calls that overlap share it.
const clients = new Map();
function walletConnectClient(projectId) {
  if (!clients.has(projectId)) {
    const made = import('../vendor/walletconnect.js').then(({ SignClient }) => SignClient.init({
      projectId,
      metadata: {
        name: 'liq2any', description: 'Trustless swaps between USDt on Liquid and USDT0 on Polygon',
        url: new URL('.', location.href).href, icons: [],
      },
    }));
    // A failed start, such as the relay being unreachable, can be tried again.
    made.catch(() => clients.delete(projectId));
    clients.set(projectId, made);
  }
  return clients.get(projectId);
}

const METHODS = ['eth_sendTransaction', 'wallet_switchEthereumChain', 'wallet_addEthereumChain'];

function walletConnectWallet(client, session) {
  const chain = NETWORK.walletConnectChain;
  const account = session.namespaces.eip155?.accounts?.find((a) => a.startsWith(`${chain}:`));
  // Since 2.2x WalletConnect asks for every namespace as optional, so a wallet can approve without sharing an account on this chain.
  if (!account) throw new Error(`${session.peer?.metadata?.name ?? 'The wallet'} connected without sharing an account on ${NETWORK.chainParams.chainName}.`);
  const address = account.split(':')[2];
  const provider = {
    request: ({ method, params }) => {
      if (method === 'eth_chainId') return Promise.resolve(`0x${NETWORK.chainId.toString(16)}`);
      return client.request({ topic: session.topic, chainId: chain, request: { method, params } });
    },
  };
  return wrap(session.peer.metadata.name, address, provider);
}

export async function connectWalletConnect(projectId, showUri) {
  const client = await walletConnectClient(projectId);
  const { uri, approval } = await client.connect({
    optionalNamespaces: { eip155: { chains: [NETWORK.walletConnectChain], methods: METHODS, events: ['chainChanged', 'accountsChanged'] } },
  });
  if (uri) showUri(uri);
  const session = await approval().catch((e) => {
    throw new Error(`Your wallet refused the connection. (${e?.message ?? e})`);
  });
  return walletConnectWallet(client, session);
}

// A session the phone approved earlier and hasn't ended, with no QR code. Null if there is none.
export async function restoreWalletConnect(projectId) {
  const client = await walletConnectClient(projectId);
  const session = client.session.getAll()
    .filter((x) => x.expiry * 1000 > Date.now() && x.namespaces.eip155?.accounts.some((a) => a.startsWith(`${NETWORK.walletConnectChain}:`)))
    .sort((a, b) => b.expiry - a.expiry)[0];
  return session ? walletConnectWallet(client, session) : null;
}
