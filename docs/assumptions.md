# liq2any: Phase 0 research

Written 2026-11-22. Every finding has a source: a document, source code (verified on Sourcify or pinned to a tag), or a read of
a live chain that anyone can repeat (section 9). Anything not checked that way is marked **UNVERIFIED** and listed again at the
end.

Prices used to convert to dollars (CoinGecko, 2026-11-22 16:23 UTC): POL $0.075675, ETH $1,715.87, SOL $70.27, TRX $0.304972.
Polygon gas price at the same time: 283.8 gwei (base fee 250.7 gwei).

**The user's decision of 2026-11-22: development starts on test networks.** Section 8 collects what exists on them. In short,
Polygon's testnet Amoy behaves like mainnet for everything the escrow needs, but USDT0 has no testnet anywhere, so the swap uses
a stand-in token there, and the onward bridge routes can't be tested end to end.

## 1. USDT0 on Polygon PoS

### 1.1 The token

| Item | Finding | Source |
|---|---|---|
| Address | `0xc2132D05D31c914a87C6611C10748AEb04B58e8F`, the old PoS-bridged USDT contract, upgraded in place to USDT0 | [USDT0 deployments API](https://docs.usdt0.to/api/deployments), key `usdt0.native`, "Polygon PoS", chain 137, EID 30109 |
| Name, symbol, decimals | `USDT0`, `USDT0`, **6** decimals. Total supply 849,264,071.80 USDT0 at block 97,772,707. | `name()`, `symbol()`, `decimals()`, `totalSupply()` |
| Conversion to Liquid | 1 base unit of USDT0 = 100 base units of L-USDt (8 decimals), as for Solana's USDT in liq2sol | derived |
| Proxy | Polygon's own `UChildERC20Proxy` (not ERC-1967). `proxyType()` = 2. Implementation `0x90040487A6C9F949C4F07CaDCFB0f3B8EEAb4229`, in storage slot `keccak256("matic.network.proxy.implementation")`. | `implementation()`, `cast storage`; [Sourcify, proxy](https://sourcify.dev/server/v2/contract/137/0xc2132D05D31c914a87C6611C10748AEb04B58e8F?fields=compilation) |
| Implementation source | `UChildUSDT0` (Solidity 0.6.6, optimizer off), verified on Sourcify. It extends Polygon's `UChildERC20` with `NativeMetaTransaction`, `ContextMixin`, Tether's `WithBlockedList` and ERC-7802 cross-chain mint and burn. | [Sourcify, implementation](https://sourcify.dev/server/v2/contract/137/0x90040487A6C9F949C4F07CaDCFB0f3B8EEAb4229?fields=sources) |
| `transfer` / `transferFrom` | Return `true`, and revert on failure (OpenZeppelin-style `ERC20.sol` in the verified source). No fee on transfer. A transfer **to the token contract itself reverts**. | `ERC20.sol` line 138; `UChildUSDT0._beforeTokenTransfer` |
| Gas | Measured with `eth_estimateGas` from a holder: `transfer` 61,722 to an address that already holds USDT0, 79,972 to a new one; `approve` 63,463. At 283.8 gwei that is $0.0013–$0.0017. | Section 9 |

### 1.2 Who can upgrade, freeze or burn

| Power | Holder | Source |
|---|---|---|
| Upgrade the token (proxy owner) | `0x4DFF9b5b0143E642a3F63a5bcf2d1C328e600bf8`, a Safe 1.4.1 with **threshold 3 of 5 owners** (`0x7D8e…aFEA`, `0x488A…434c`, `0x00F6…78F2`, `0x4192…6ac5`, `0x5F0D…b982`). USDT0's deployments list names it "Safe". | `proxyOwner()`; Safe `getThreshold()`, `getOwners()`, `VERSION()` |
| `DEFAULT_ADMIN_ROLE`: `mint`, `redeem`, `destroyBlockedFunds`, `setOFTContract`, `changeName` | The same Safe, alone | `getRoleMember(0x00…, 0)`, count 1 |
| `BLOCK_ROLE`: `addToBlockedList`, `removeFromBlockedList` | The same Safe, alone | `getRoleMember(BLOCK_ROLE, 0)`, count 1 |
| `DEPOSITOR_ROLE`: `crosschainMint`, `crosschainBurn` | The USDT0 OFT `0x6BA10300f0DC58B7a1e4c0e41f5daBb7D7829e13`, alone | `oftContract()`, `getRoleMember(DEPOSITOR_ROLE, 0)` |
| Who the five Safe owners are | Five plain keys: none has contract code. Only `0x7D8e…aFEA` has sent transactions on Polygon (nonce 2). The Safe at the same address on Arbitrum has the same five owners and threshold. Who holds them is not published: USDT0's [security page](https://docs.usdt0.to/technical-documentation/security.md) lists audits, not signers. **UNVERIFIED.** | `cast code`, `cast nonce`; Arbitrum `getOwners()`, `getThreshold()` |

What a block does, from the verified source:

- A blocked address can't send: `_beforeTokenTransfer` requires `!isBlocked[from]` (unless the admin itself is the caller).
- A blocked caller can't call `transferFrom` (`onlyNotBlocked` on `_msgSender()`).
- **Receiving is not blocked**: nothing checks `to`. A blocked recipient still receives, and then can't move the coins.
- `destroyBlockedFunds` burns a blocked address's whole balance.

**What this means for an escrow contract.** If the escrow holds every swap's coins in one contract, a block on that contract
freezes every open swap at once, and `destroyBlockedFunds` could burn them all. Each swap holding its coins at its own address
(a minimal clone, or a CREATE2 vault per swap) limits a block to one swap. This is a Phase 1 choice. liq2sol's Solana escrow
already holds each swap in its own token account, which Tether can freeze one by one.

### 1.3 Approvals without a separate transaction

| Item | Finding | Source |
|---|---|---|
| `permit` | Present, **classic `v, r, s` only**: `permit(owner, spender, value, deadline, v, r, s)`. Type hash `Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)`, as in EIP-2612. EOA signatures only, no EIP-1271. | `UChildUSDT0.permit`; [USDT0 token features](https://docs.usdt0.to/technical-documentation/token-features.md) |
| **Non-standard signing domain** | `EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)`, with `name = "USDT0"`, `version = "1"`, `salt` = the chain id as bytes32, and **no `chainId` field**. A permit signed with the usual domain fails. Computed `0x7b43b7de…9a05` and on-chain `DOMAIN_SEPARATOR()` match. | `EIP712Base.sol`; USDT0's [signing domain](https://docs.usdt0.to/technical-documentation/token-features.md#signing-domain) section; section 9 |
| `deadline = 0` | Means "never expires" (`deadline == 0 \|\| now <= deadline`). A page must never ask for one. | `UChildUSDT0.permit` |
| Shared nonce | `permit` and the meta-transactions below increment **the same** `nonces[owner]`, so either one invalidates a pending signature of the other. | `NativeMetaTransaction.nonces`, used by `permit` |
| EIP-3009 (`transferWithAuthorization`) | **Not on Polygon**, though it is on most other USDT0 chains | USDT0's availability matrix; absent from the verified ABI |
| Polygon native meta-transactions | `executeMetaTransaction(user, functionSignature, r, s, v)`: anyone may submit a call that `user` signed (EIP-712 type `MetaTransaction(uint256 nonce,address from,bytes functionSignature)`), and the token treats `user` as the caller. So a relayer can submit a taker's `approve` or `transfer` with no POL in the taker's wallet. | `NativeMetaTransaction.sol`, `ContextMixin.sol` |
| Wallets signing a domain with `salt` | **Works with MetaMask's own signing library.** On a local fork of Polygon, a permit signed by `@metamask/eth-sig-util` 8.2.0 (`signTypedData` V4, which backs MetaMask's `eth_signTypedData_v4`) with the salt domain was accepted, and so was one signed by viem 2.21.55. One signed with the usual `chainId` domain reverted. `permit` costs 64.4k gas. Whether every wallet's own screen accepts and shows a salt domain: **UNVERIFIED**. | Section 9 |

### 1.4 The OFT on Polygon

| Item | Finding | Source |
|---|---|---|
| Address | `0x6BA10300f0DC58B7a1e4c0e41f5daBb7D7829e13`, an ERC-1967 `TransparentUpgradeableProxy` (implementation `0x8412d655…ae47`, proxy admin `0xa882c21c…15b1`) | USDT0 deployments API; ERC-1967 slots; [Sourcify](https://sourcify.dev/server/v2/contract/137/0x6BA10300f0DC58B7a1e4c0e41f5daBb7D7829e13?fields=compilation) |
| `owner()` | The same 3-of-5 Safe | `owner()` |
| Endpoint | `0x1a44076050125825900e736c501f859c50fE728c`, which matches `polygon-mainnet` EID 30109 in LayerZero's metadata | `endpoint()`; [LayerZero deployments](https://metadata.layerzero-api.com/v1/metadata/deployments) |
| `approvalRequired()` | **false**: the OFT burns through `crosschainBurn`, so sending onward from Polygon needs no `approve` | `approvalRequired()` |
| Protocol fee | **None.** `quoteOFT` for 1,000 USDT0 to Arbitrum gives no fee entries and receives exactly 1,000. Limits 0 to 2⁶⁴−1. | `quoteOFT` |
| Shared decimals | 6, the same as the token, so no dust is lost | `sharedDecimals()` |

## 2. The secret check on Polygon

### 2.1 What liq2sol checks today

The Solana escrow stores the 64-byte point `T` (x then y) as `lock_point`. A claim reveals `s = t·Gx mod n`. The program calls
`secp256k1_recover` with hash 0, recovery id 0 and signature `(r = Gx, s)`. Recovery computes `r⁻¹(s·R − z·G)`, and with
`R = G` (recovery id 0, since G's y is even) and `z = 0` that is `(s/Gx)·G = t·G`. The program compares the recovered key with
`lock_point`. Anyone then computes `t = s/Gx mod n` off-chain (`reveals_point` in `programs/solana-escrow/src/lib.rs`;
`solana_value`, `secret_from_solana` and `solana_point_of` in `contracts/swapkey/src/lib.rs`).

### 2.2 The same check on Polygon

EVM's `ecrecover` precompile computes the same recovery, but returns an **address**, the last 20 bytes of `keccak256(x ‖ y)`,
instead of the point. So the escrow stores `lockAddress = address(T)` and checks

```solidity
lockAddress != address(0) && ecrecover(bytes32(0), 27, GX, s) == lockAddress
```

`v = 27` selects the even-y `R`, which is `G`, as recovery id 0 does on Solana. Everything else in liq2sol carries over: the same
`t`, the same `s = t·Gx mod n`, and the adapter signature on Liquid stays bound to `T`. Each party checks that the escrow's
`lockAddress` is `address(T)` for the `T` its adapter signature uses.

Checked on the chains themselves, by calling the precompile at `0x01` directly on Polygon's own nodes (section 9):

| Case | Mainnet (Bor v2.11.1) | Amoy (Bor v2.11.0) |
|---|---|---|
| Right `s` for a random `t` | Returns `address(T)` = `0x73d8…d207` | Same |
| `t = 1` (so `s = Gx`) | Returns `0x7e5f4552091a69125d5dfcb7b8c2659029395bdf`, the well-known address of private key 1 | Same (Foundry) |
| Same `s`, `v = 28` | Returns another address (that of `−T`) | Same |
| `s = 0` | Empty output, so Solidity's `ecrecover` gives `address(0)` | Same |
| `s = n` | Empty output | Same |
| A wrong `s` | Returns an unrelated address | Same |

**Gas: 3,736 for the whole check inside a contract** (Foundry gas report, Solidity 0.8.30, optimizer on), of which 3,000 is the
precompile. At 283.8 gwei that is 0.00106 POL, about $0.00008. A direct call to the precompile estimated 25,633 gas, which is
the 21,000 base, calldata and 3,000.

Rules this sets for the contract (Phase 1 adds tests for each):

- **Refuse a zero `lockAddress` at lock time.** Otherwise `s = 0` would open the escrow, since a failed recovery returns
  `address(0)`.
- **Hard-code `v = 27`.** With 28 the check accepts `−t·Gx` for `−T`, which is still the counterparty's secret, but the code must
  never accept two values.
- Only 20 bytes of `keccak256(T)` are stored. Finding another point with the same address is a 160-bit second-preimage problem.
- **Privacy is slightly better than on Solana:** `T` itself never appears on-chain before the claim, only its hash, while `s`
  appears at the claim as on Solana. Neither links to Liquid, for the reason given in liq2sol's `docs/confidential.md`.
- A claim pays a recipient fixed at lock time, so anyone may submit it. Copying `s` from the mempool gives a front-runner nothing
  on Polygon. It still gives the counterparty `t` early, exactly as on Solana, so liq2sol's rule stands: reveal only after checking
  the counterparty's lock on two independent endpoints and simulating the claim, and inside the cutoff margin.

## 3. Polygon PoS as a swap chain

### 3.1 Block time and finality

| Item | Finding | Source |
|---|---|---|
| Block time | **Mainnet 1.5 s**: the last 10,000 blocks averaged 1.50 s, and timestamps alternate +1 s and +2 s. **Amoy 1.0 s.** Polygon has changed it in upgrades (Rio introduced flexible block times), so **never hard-code it**: the lesson of Solana's slots. | Section 9; Bor `consensus/bor/bor.go` v2.11.2, `Prepare` |
| Finality | Deterministic, from Heimdall **milestones**: validators vote on Bor block hashes, and the longest sequence with ⅔ agreement becomes final. Polygon says 2–5 s, and "no chance of a reorganization once a block is finalized". | [Polygon finality docs](https://docs.polygon.technology/pos/concepts/finality/finality); [PIP-11](https://github.com/maticnetwork/Polygon-Improvement-Proposals/blob/main/PIPs/PIP-11.md) |
| Reading it | `eth_getBlockByNumber("finalized")`. Bor has no `safe` block ("safe block not found" on both networks). | Same docs; section 9 |
| Measured lag | `finalized` trailed `latest` by 1–3 blocks on mainnet (mean 1.6, 30 samples over 60 s) and by 1–4 on Amoy (mean 1.9), so about 2–5 s | Section 9 |
| Checkpoints to Ethereum | Still exist, for bridge withdrawals. The interval is a governance parameter. Irrelevant to the swap. | Polygon finality docs |

### 3.2 Incidents, measured on the chains

Reports of Polygon's incidents disagree with each other, so each was measured, two ways:

- **Halts:** the largest gap between consecutive Bor block timestamps, in a three-day window around each date.
- **Finality stalls:** the gap between consecutive Heimdall milestones, whose timestamps are Heimdall's own consensus time. Those
  timestamps also give an independent clock to compare with Bor's.

| When | What the chains show | What was reported |
|---|---|---|
| **2022-03-10** | **A real halt: 15,148 s (4 h 12 min) with no block**, ending with block 25,811,392 at 17:54:16 UTC. The timestamp jumped by the whole halt. | Heimdall v0.2.5 let a large transaction clog state sync, and Bor halted until a hotfix. No duration given. [Polygon forum postmortem](https://forum.polygon.technology/t/postmortem-polygon-pos-outage-learnings/2745) |
| 2025-07-30 | No timestamp gap over 30 s. **Finality stalled 8.7 minutes** (milestones at 11:00:37 and 11:09:20 UTC), and blocks came up to 38% slower for several hours (1,020–1,470 an hour, against about 1,650). | "More than two hours" with no blocks ([Finway](https://finway.com.ua/en/polygon-pos-network-experiences-outage/), secondary). **The chain doesn't support that.** |
| **2025-09-10** | **Finality stalled 10 h 37 min**: no milestone from 04:29:46 to 15:06:14 UTC. Blocks kept coming, about 13,400 of them at about 2.85 s each, against 2.2 s normally. | "Finality delays of 15 minutes up to 1 hour" in an incident of about 12 hours, fixed by an emergency hard fork ([Metrika](https://www.metrika.co/blog/post-mortem-polygon)). The shorter figure probably reflects Bor's `finalized` falling back to Ethereum checkpoints when it has no milestone ([bor PR #2226](https://github.com/0xPolygon/bor/pull/2226)). |
| **2025-12-17** | **Finality stalled 4 h 57 min**: no milestone from 18:29:20 to 23:25:53 UTC, then catching up within 35 minutes. Blocks came 15–37% slower meanwhile (1,130–1,680 an hour, against about 1,790). | A faulty validator proposal forked Bor nodes; Polygon's co-founder said it "temporarily halted production" ([Cryptopolitan](https://www.cryptopolitan.com/polymarket-l2-polygon-network-disruption/), secondary). The chain shows no halt. |
| 2026-09-19 | Nothing: 2,388–2,400 blocks an hour, and Bor's clock within 2 s of Heimdall's throughout. | "Degraded Performance on Block Producer", delayed confirmations, fixed by rotating the producer ([Polygon status](https://status.polygon.technology/incidents/01M2XMCZJ4EXFC5Q2AH7XQ0NDD)). It probably delayed inclusion or RPC, not blocks. |

In every event the measured Bor clock and Heimdall clock agreed outside the stalls, so **Bor's timestamps have tracked real time**.

What this means:

- **The last full halt found was in 2022, about 4 hours.**
- **Finality stalls are the real risk: 10.6 hours in September 2025, 5 hours in December 2025.** Blocks kept coming, so a party
  that acts only on finalized blocks (as it must, section 3.4) waits that long.
- Neither is near Liquid's 4-day stoppage of September 2026, but Polygon's history is no promise.

Polygon's status page keeps history only from August 2026, so older incidents were found from the reports above, and others may
have been missed.

### 3.3 One block producer at a time

Since the Rio upgrade (mainnet about 2025-10-08), Polygon has a single elected block producer per span (PIP-64, "VEBloP"), with
backups that take over when it stalls ([Polygon blog on Rio](https://polygon.technology/blog/polygon-launches-major-payments-upgrade-with-rio-faster-lighter-and-easier-to-build)).
Heimdall's API confirms it:

- `bor/params` gives `span_duration` = 6,400 blocks, about **2.7 hours** at 1.5 s, and `sprint_duration` = 16;
- `bor/spans/latest` (span 15,525) has a validator set of 100 and **one `selected_producers` entry** (validator 91).

The Ithaca upgrade (Polygon's blog, 2026-07-30, mainnet Heimdall block 50,185,000) rotates in a healthy producer automatically
when the current one stalls; before it, a stall could last about 15 minutes with no automatic recovery
([Polygon blog on Ithaca](https://polygon.technology/blog/ithaca-upgrade-is-live-payments-on-polygon-chain-are-more-reliable-than-ever)).
The blog gives no stall threshold.

What this means for swaps:

- **A producer can censor a claim or a refund for up to its span, about 2.7 hours.** Ithaca rotates on a stall. Nothing found
  rotates a producer that keeps producing but leaves certain transactions out: **UNVERIFIED** whether anything does. The deadlines'
  margins must outlast at least one span.
- **The producer sets `block.timestamp`**, within limits read from Bor v2.11.2 (`verifyHeader` and `Prepare` in
  `consensus/bor/bor.go`):
  - at most 30 s ahead of each validating node's clock (`maxAllowedFutureBlockTimeSeconds = 30`);
  - at least the parent's time plus the period (1 s), and **no lower bound against the clock**;
  - an honest producer whose slot has already passed sets the time to now plus the block time, so **after a halt the timestamp
    jumps to the wall clock**, while block numbers simply resume. The March 2022 halt shows exactly this on the chain: one gap
    of 15,148 s (section 3.2). In the events since, Bor's clock stayed within seconds of Heimdall's.

  So a dishonest producer could run the clock slow, at worst +1 s per block, which is about two-thirds of real speed at 1.5 s
  blocks, and could skip the jump after a halt.

### 3.4 Halts, in both directions

As in liq2sol, the taker always locks first, so the taker's lock is the long leg and the maker's the short leg. "Halts are
asymmetric": a halt on the chain holding the short leg is the dangerous one. The table uses liq2sol's terms: `LOCK_HEIGHT` is the
Liquid refund height, and the Polygon escrow's deadline is open to design. Deadlines can count in **blocks** (they pause in a halt)
or **seconds** (they jump after it, if the producer is honest).

| Direction and halt | What happens | With a block deadline | With a time deadline |
|---|---|---|---|
| L→P (Liquid long, Polygon short), **Liquid halts** | `LOCK_HEIGHT` can't pass. The maker claims on Liquid when blocks resume. | Safe, delayed | Safe, delayed |
| **L→P, Polygon halts, both locked** | Liquid's clock runs on. If `LOCK_HEIGHT` passes during the halt, the taker refunds on Liquid, then claims on Polygon after the restart. | **Theft**, as Solana's L→S without `close_on_restart` | **Mostly closed**: the timestamp jumps after the restart, so the claim window ends at its real time. Residual: a dishonest producer slowing the clock or skipping the jump. |
| P→L (Polygon long, Liquid short), Polygon halts | The taker claims on Liquid, revealing `t`, and the maker must then claim on Polygon before the deadline. | Safe: the deadline pauses with the blocks | **Theft** if the halt eats the maker's margin: the timestamp jumps past the deadline and the taker refunds. No `extend` can help, because nothing lands during a halt. |
| **P→L, Liquid halts, both locked** | Polygon's clock runs on. The taker refunds on Polygon at the deadline, then claims on Liquid when blocks resume, before `LOCK_HEIGHT`. | **Theft** without mitigation, as liq2sol's S→L | **Theft** without mitigation |

Conclusions for Phase 1:

- **The short leg on Polygon (L→P) wants a time deadline. The long leg (P→L) wants a block deadline**, and the maker's `extend`
  for a Liquid halt, as liq2sol's D1. One contract can do both: a claim stays open while *either* bound holds for a long leg and
  only while *both* hold for a short leg. That keeps a block-time change from shortening a long leg. Phase 1 works out the details.
- Block deadlines must be computed from the measured block rate, with margin, never from a constant.
- Act only on `finalized` blocks. The September 2025 event rolled back nodes that had followed a bad milestone.
- **Finality stalls of 5–11 hours have happened** (section 3.2). During one, the chain runs on but nobody should act on it. That
  is a halt in all but name for whoever waits on finality, and the margins and the maker's halt detection must treat it as one.
  How Bor's `finalized` behaves during a stall (stopped, or following Ethereum checkpoints) decides what the maker sees:
  **UNVERIFIED**, but the bor PR above says it falls back to the latest checkpoint.
- Halt detection for the maker: stop new swaps when Polygon's `finalized` block stops moving for a set time (liq2sol uses 60 s
  for Solana).

## 4. Gas for the taker on Polygon

What a taker sends on Polygon, at 283.8 gwei:

Measured as separate transactions on a local fork of Polygon mainnet, against the real USDT0 contract. The escrow was a measurement
probe of about 30 lines: a lock storing three slots and pulling the tokens, and a claim checking `s` and paying out. Phase 1's
contract will differ a little.

| Step | Gas | Cost |
|---|---|---|
| `approve` (P→L) | 51,650 | 0.0147 POL, $0.0011 |
| `lock` (P→L) | 157,873 | 0.0448 POL, $0.0034 |
| `permit` instead of `approve` (section 1.3) | 64.4k, but it can share a transaction with the lock | — |
| `claim`, sent by a third party, to a recipient new to USDT0 (L→P) | 73,403 | 0.0208 POL, $0.0016 |
| A second claim | Reverts | — |
| An onward bridge send (section 5) | 515–566k, measured on mainnet, plus the LayerZero fee | 0.15 POL ($0.011) plus 0.006–180 POL, by route |

The cost is negligible. **The problem is that the taker needs POL at all**, and a Liquid user usually has none. The options:

| Option | How | Trust and risk | Code |
|---|---|---|---|
| **A. The taker holds POL** | The page asks for it | None added. Worst for onboarding: the taker needs another swap first. | None |
| **B. A gas drop from the maker** | The maker attaches a little POL to its L→P lock, paid to the recipient with the claim, or sends it beside the lock. Sized to cover the claim and, if wanted, the onward bridge fee (which must be POL anyway). | None for the taker: it's a gift, priced into the quote. The maker loses the drop if the taker never claims, unless the drop refunds with the lock. | Small: a `payable` lock |
| **C. Anyone may claim (L→P)** | The claim is permissionless, since it pays the fixed recipient. The taker hands `s` to the maker or any relayer. An optional tip, fixed at lock time and paid to `msg.sender` from the escrow, makes submitting worth anyone's while. | Giving out `s` gives the maker `t`, which it would get anyway when the claim lands. **If nobody submits before the deadline, the taker loses**, unless it can submit itself. So this is liveness, not safety, and works best with B as the fallback. | Small |
| **D. Signed lock, relayed (P→L)** | The taker signs a `permit` for the escrow and an EIP-712 lock order (point, recipient, amount, deadline, expiry). The maker submits `lockWithPermit`, paying the gas. Polygon's `executeMetaTransaction` on the token is an alternative to `permit`. | The signed terms fix everything, so the maker can't change them. **But the maker chooses when to submit, until the order expires**, so the page must treat the swap as live until then and keep the expiry short. `permit` and meta-transactions share one nonce. | Medium: signature checks in the contract |
| **E. Inbound with a native drop** | A taker bridging into Polygon asks LayerZero to deliver POL too (executor option `lzNativeDrop`). Quoted live: Arbitrum → Polygon costs 0.0000088 ETH ($0.015), or 0.0000856 ETH ($0.147) with 1 POL dropped. | None added beyond the bridge. LayerZero Scan shows native drops `SUCCEEDED` on recent USDT0 messages to Arbitrum (3) and Ethereum (1). None into Polygon was seen: **UNVERIFIED** there. | Small, in the page |
| F. EIP-7702 or ERC-4337 paymasters | Sponsored transactions through smart accounts | EIP-7702 is live on Polygon since the Bhilai hard fork at block 73,440,256 (Bor upgraded to geth 1.15, [PIP-61](https://polygon.technology/blog/first-milestone-to-gigagas-1000-tps-with-bhilai-hardfork)). Which wallets let a page use it, and which paymasters: **UNVERIFIED**. A paymaster service is a new dependency. | Large |

The onward bridge fee is always paid in POL (section 5), so a taker who continues to another chain needs POL whatever the claim
does. That argues for B, sized per route.

## 5. Onward routes from Polygon, priced on mainnet

### 5.1 How each target is reached

- **USDT0 chains** (Ethereum's adapter, Arbitrum, Optimism, Mantle, Plasma, HyperEVM, Monad, Unichain, Ink and others in the
  deployments list): one LayerZero message from the Polygon OFT. Burn on Polygon, mint (or unlock, on Ethereum) at the
  destination. No protocol fee. All have peers set (`peers(eid)`).
- **Solana, Tron, TON and Celo** (the Legacy Mesh): **no direct peer from Polygon** (`peers()` is zero for 30168 and 30420, and
  `quoteSend` reverts). They go in **two hops through Arbitrum**: Polygon's OFT sends to the **MultiHop Composer** on Arbitrum
  (`0x759BA420bF1ded1765F18C2DC3Fc57A1964A2Ad1`), with an ABI-encoded `SendParam` for hop 2 as the compose message. The composer
  forwards to the Legacy Mesh adapter (`0x77652D5aba086137b595875263FC200182919B92`), which pays from its pool, less 0.03%.
- **Base is not a USDT0 chain** and has no Tether-issued USDT, so it is out of reach for this design (it is absent from the deployments list).

The composer (verified on Sourcify, chain 42161), from its source:

- **Hop 2 is paid by native value that the sender attaches to hop 1's `lzCompose` option.** The sender quotes hop 2 in ETH on
  Arbitrum, then quotes hop 1 on Polygon with that value in the compose option, and pays it all in POL.
- It overrides `amountLD` with what arrived and sets `minAmountLD = 0`. Slippage is impossible here, but the 0.03% fee is taken
  without any check against the sender's own minimum.
- **If hop 2 fails, anyone may call `refund(guid)`, which sends the coins back to the original sender on the source chain**, or
  `retry(guid)` when hop 2 itself failed. A message that fails to decode can only be refunded. So **the escrow contract must never
  be the sender of a bridge leg**: a refund would come back to the escrow. The taker's own wallet sends.

### 5.2 Who verifies

Read from the endpoint's ULN config for each path of the Polygon OFT:

- **Every path has the same 3 required verifiers, all of which must sign: LayerZero Labs, USDT0 and Canary Sponsored.** No
  optional verifiers.
- Every send from Polygon waits for **32 Polygon confirmations**.
- Messages into Polygon wait for the source chain's confirmations:

  | From | Confirmations |
  |---|---|
  | Arbitrum | 60 |
  | Ethereum | 65 |
  | Flare | 500 |
  | Optimism | 1,000 |
  | Unichain | 1,600 |
  | Plasma | 1,800 |
  | Mantle | 2,000 |
  | Ink | 3,000 |
  | Monad | 3,600 |
  | HyperEVM | 43,200 |

Names come from [LayerZero's DVN metadata](https://metadata.layerzero-api.com/v1/metadata/dvns). The Legacy Mesh's own hops use
LayerZero Labs and USDT0 (sol2tron, `docs/assumptions.md` section 1.3).

### 5.3 What it costs and how long it takes

Live `quoteSend` and `quoteOFT` for **1,000 USDT0**, 2026-11-22 about 16:25 UTC. The LayerZero fee is in POL on Polygon. "Taken"
is what the bridge keeps from the amount. Times are source block to destination block, from LayerZero Scan's 50 most recent
messages of the Polygon OFT.

| From Polygon to | Hops | LayerZero fee | In USD | Taken | Measured time |
|---|---|---|---|---|---|
| Arbitrum | 1 | 0.3600 POL | $0.027 | 0 | 53–66 s (about 30 samples) |
| Ethereum | 1 | 1.2435 POL | $0.094 | 0 | 75–114 s (2) |
| Optimism | 1 | 0.0060 POL | < $0.001 | 0 | 59–61 s (2) |
| Plasma | 1 | 3.2×10⁻¹⁰ POL | ≈ $0 | 0 | 59–65 s (6) |
| Ink | 1 | 0.0109 POL | $0.001 | 0 | 56 s (1) |
| Mantle, HyperEVM, Monad, Unichain | 1 | 0.195, 0.043, 0.013, 0.020 POL | $0.015, $0.003, $0.001, $0.001 | 0 | not seen |
| **Solana** | 2 | **8.83 POL** | **$0.67** | **0.30 USDT** (0.03%) | **71–73 s** end to end (3) |
| **Tron** | 2 | **179.66 POL** | **$13.60** | 0.30 USDT | **107 s** (1) |
| TON | 2 | 1.875 POL | $0.14 | 0.30 USDT | not seen |
| Celo | 2 | 4.496 POL | $0.34 | 0.30 USDT | not seen |

- Hop 2 alone, quoted on Arbitrum in ETH: Solana 0.000308 ETH ($0.53), Tron 0.00657 ETH ($11.28), TON 0.000053 ETH ($0.09),
  Celo 0.000149 ETH ($0.26). Tron is expensive for the reason sol2tron found: the executor buys Tron energy to deliver.
- The two-hop quotes add 1 gas to the compose option on top of the 100,000 the OFT enforces (`enforcedOptions(30110, 2)` =
  lzReceive 100,000 plus lzCompose 100,000), because a zero option is refused. The compose value was set at the hop-2 quote,
  with no margin. Too little fails hop 2 (then `retry` or `refund`, section 5.1). Too much is not returned to the taker: the
  composer passes `tx.origin` as the refund address, and on Arbitrum that is whoever executed the compose, normally LayerZero's
  executor. So the margin is a cost.
- Plasma's near-zero fee is real: a mainnet send that paid 0.00000000029 POL (tx `0x8b30955f…c3b9`) was delivered.
- **Polygon gas for the send itself is 515–566k gas**, read from the receipts of seven recent sends, one of them two-hop. That is
  about 0.15 POL, $0.011.
- Those sends paid about what the quotes say. Ethereum paid 1.266 POL against a 1.24 quote, and Ink 0.01094 POL, exactly the
  quote. A two-hop send to Solana paid 27.07 POL, at a gas price of 491 gwei and with its own margin.

**Capacity of the Legacy Mesh** (Arbitrum adapter's `credits(eid)`, the same moment), which caps hop 2:

| To | Credits |
|---|---|
| Solana | 83,286.94 USDT |
| Tron | 3,700,996.05 USDT |
| TON | 106,329.39 USDT |
| Celo | 305,119.16 USDT |
| Ethereum's legacy pool | 0.000001 USDT, empty. Irrelevant here: Polygon reaches Ethereum directly through the USDT0 adapter. |

A hop 2 over the credits fails on Arbitrum, and the composer then holds the coins for `refund` back to Polygon or `retry`. So
**the page must read `credits` before the taker sends**, and the result is a delay, not a loss.

**Inbound to Polygon is slower.** Measured from LayerZero Scan:

- Ethereum → Polygon took 855–1,194 s (11 messages);
- Arbitrum → Polygon took 925–1,331 s (6 messages, from the Arbitrum OFT's last 100);
- Optimism → Polygon messages were still `INFLIGHT`.

So a P→L taker starting on another chain waits about 15–22 minutes before the swap can start, and anything routed through Arbitrum
pays that on its second hop. No inbound transfer from Solana or Tron through the composer appeared in the last 100 messages of
either chain's OFT, so its total time is **UNVERIFIED**; it should be hop 1 plus the 15–22 minutes above.
sol2tron measured Solana's and Tron's own sends: Solana → Arbitrum 0.001–0.002 SOL, while a Tron sender pays 6–39 TRX in energy
and fees.

### 5.4 Who can stop a transfer

- **USDT0's 3-of-5 Safe** owns the token, the OFT and the peers. It can upgrade them, block addresses, burn blocked funds and
  change peers or options (section 1.2).
- **Any one of the three verifiers** (LayerZero Labs, USDT0, Canary) can stall the Polygon ↔ Arbitrum path by not verifying.
  None can release funds alone.
- **The Legacy Mesh's `burnAndNilifyAdmin`** can cancel an inbound mesh message. What happens to its money is **UNVERIFIED**
  (sol2tron).
- Once sent, there is **no refund at the source** for a single hop. A stuck message is executed late, or not at all.
- **Anyone may execute a verified message**, so the page could offer it. Two sources show this:
  - LayerZero's `EndpointV2.lzReceive` (verified source at the Polygon endpoint) has no caller check;
  - the USDT0 OFT implementation (`OUpgradeable`, verified source of `0x8412d655…ae47`) checks only that the endpoint calls it
    (`OAppReceiverUpgradeable`), and ignores the executor's address (`OFTCoreUpgradeable._lzReceive`).
- A two-hop transfer whose second hop fails can be refunded to Polygon (5.1).
- LayerZero Scan's statuses, from its [API reference](https://docs.layerzero.network/v2/tools/layerzeroscan/testnet/messages/get-messagesstatus):
  - `INFLIGHT`: waiting for source confirmation, verification or execution;
  - `CONFIRMING`: destination transaction submitted, waiting for finality;
  - `DELIVERED`: executed;
  - `FAILED`: delivered but execution failed;
  - `BLOCKED`: can't progress, because of configuration, or an earlier nonce's stored payload;
  - `PAYLOAD_STORED`: arrived but reverted or ran out of gas, and needs a retry (from older Scan docs);
  - `APPLICATION_BURNED`: undefined in the docs found, **UNVERIFIED**.

  This run observed `DELIVERED`, `INFLIGHT` and, for destination sub-steps, `SUCCEEDED`.

## 6. Wallets

| Item | Finding | Source |
|---|---|---|
| eth2base's reference | Injected wallet first (`window.ethereum`, `eth_requestAccounts`), else WalletConnect through `@walletconnect/ethereum-provider` 2.17.2 with the page's own QR code and buttons, never Reown's modal. One provider per page, because two cores on the same storage dropped wallet answers. viem 2.21.55 from esm.sh. | `~/src/eth2base/app.js` lines 740–860 |
| liq2sol's client | Vendors `@walletconnect/sign-client` 2.26.0 (`client/vendor/walletconnect.js`), one client per project, and asks for the `solana` namespace as optional. An `eip155` namespace (`eip155:137`, `eip155:80002`) would go into the same client. Whether one session carries both `solana` and `eip155` with common wallets: **UNVERIFIED**. | `~/src/liq2sol/client/site/js/wallet.js` lines 72–140 |
| **No signing without sending** | MetaMask refuses `eth_signTransaction`; EVM wallets generally only offer `eth_sendTransaction`, which signs and broadcasts. So **the page can't hold a signed Polygon claim and choose when `s` goes out**, as liq2sol does with `solana_signTransaction`. Since the claim is permissionless (section 2.2), the page can instead simulate with `eth_call` first, or hand the claim to a relayer (section 4, option C). | [ethers v5 docs](https://docs.ethers.org/v5/api/signer/); [Hardhat discussion #2271](https://github.com/NomicFoundation/hardhat/discussions/2271) |
| Typed data | `eth_signTypedData_v4`, for `permit` with Polygon's salt domain and for a signed lock order | EIP-712; section 1.3 |
| Extension discovery | eth2base uses `window.ethereum` only. EIP-6963 lets a page with several extensions installed list them and pick one. MetaMask, Coinbase Wallet and Phantom announce through it, per the WalletConnect (Reown) list of 2023, and Rabby per later secondary sources. Not tested here. | [Reown blog](https://reown.com/blog/eip6963-is-now-approved); [DEV article](https://dev.to/grimicorn/connecting-wallets-the-right-way-wagmi-v2-and-eip-6963-4k02) |

## 7. The alternative: inventory on every target chain

The question is whether the maker should hold USDT on each target chain, with an escrow on each, so that every leg is atomic.

| | **Hub** (this brief): Liquid and Polygon only | **Inventory everywhere** |
|---|---|---|
| Atomic legs | Liquid ↔ Polygon only. Onward legs trust USDT0, LayerZero and the verifiers. | Every leg |
| Taker's cost per swap, beyond the core swap | The bridge: about $0.03–$0.67 to EVM chains and Solana, **$13.60 to Tron**, plus 0.03% through the mesh (section 5.3) | Gas for one claim on the target chain: cents on EVM chains, about $9–12 of TRX energy on Tron with no staked energy (sol2tron) |
| Maker's capital | Split two ways | Split N+1 ways, each with its own float, for the same depth |
| Who carries bridge risk | The taker, per transfer, by choice | The maker, when it rebalances, in bulk and at its own timing. The bridges are still used. |
| Escrow code | One EVM contract | **The same EVM contract works on every EVM chain with `ecrecover`**, deployed per chain. Solana already has liq2sol. Tron's TVM has `ecrecover` at `0x01` at 3,000 energy, and TRON's Solidity compiler tracks upstream ([TVM vs EVM](https://developers.tron.network/docs/tvm-vs-evm)), so the same contract likely ports. That TVM's recovery accepts this input (hash 0, `r = Gx`): **UNVERIFIED** until called. TON needs a new contract (a usable secp256k1 recovery there: **UNVERIFIED**). |
| Per-chain work | None per target | For each chain, a finality and halt analysis like section 3, its own gas token for the maker, its own freeze rules, and endpoints in the page |
| Maker operations | Two balances, one rebalance path | N balances, each watched, each with a gas token |

There is a middle way that Phase 1 should weigh: the hub as described, with atomic escrows added later on the EVM chains where
volume justifies inventory, since the contract would be the same. liq2sol already covers Solana atomically.

How the maker rebalances between Liquid and Polygon:

- **Bitfinex holds both.** It withdraws USDt on Liquid ([Blockstream help](https://help.blockstream.com/hc/en-us/articles/900000642443-Acquire-LBTC-and-USDT-with-Bitfinex)),
  and since December 2025 it deposits and withdraws USDT0 on Polygon (`USDT0POL`).
- So a maker can deposit on one network and withdraw on the other, at the cost of an exchange account with KYC, custody while the
  coins are there, and a $5 minimum.
- Third-party fee trackers list Liquid withdrawals as free, and Bitfinex as charging only for USDT on Ethereum. Current fees from
  Bitfinex itself: **UNVERIFIED**.
- Other routes (Tether redemption, SideSwap, other exchanges): **UNVERIFIED**, for Phase 1.

## 8. Test networks

| Item | Finding | Source |
|---|---|---|
| Polygon Amoy | Chain id 80002, Bor v2.11.0, 1.0 s blocks, `finalized` 1–4 blocks behind, no `safe` block. Gas price 48.9 gwei on 2026-11-22. | Section 9 |
| The secret check on Amoy | Same results as mainnet for every case in section 2.2 | Section 9 |
| **USDT0 on testnets** | **None.** No testnet entries in USDT0's deployments API or docs. A third-party project deployed its own test OFT for the same reason. So the swap on Amoy uses **a stand-in ERC-20 that the project deploys**, as liq2sol's devnet uses its own mint and faucet. To mimic Polygon's token, the stand-in can copy its `permit` with the salt domain. | [Deployments API](https://docs.usdt0.to/api/deployments); [Writz issue #314](https://github.com/WritzProtocol/writz/issues/314) |
| LayerZero on Amoy | Endpoint V2 `0x6EDCE65403992e310A62460808c4b910D972f10f`, EID 40267, with contract code on Amoy. Solana testnet is EID 40168 and Tron testnet 40420. | [LayerZero deployments](https://metadata.layerzero-api.com/v1/metadata/deployments); `cast code` |
| Onward routes on testnets | Can't be tested end to end: there is no USDT0 or Legacy Mesh on any testnet. The page's bridge code can be exercised against our own test OFT on Amoy and another testnet, or against mainnet with small amounts once the user decides to. | — |
| POL on Amoy | Polygon's docs list five faucets: Polygon's own (`faucet.polygon.technology`), Alchemy (0.2 POL a day, 0.5 with an account), QuickNode (needs a connected wallet), GetBlock (needs an account) and StakePool. Search results add that some ask for a mainnet balance. None was tried. The test maker could drop POL to takers (section 4, option B), which also tests that option. | [Polygon faucet docs](https://docs.polygon.technology/tools/gas/matic-faucet) |
| Public Amoy RPC | `https://polygon-amoy-bor-rpc.publicnode.com` answered every call here. Polygon's own `rpc-amoy.polygon.technology` **no longer resolves** (2026-11-22), which fits the reported retirement. | Section 9 |
| Liquid testnet | As in liq2sol: the public Esplora, and the PEGx test USDt `b612eb46…9d73` | liq2sol `docs/assumptions.md` |
| Circle USDC on Amoy | `0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582`, only relevant if USDC enters scope | [Circle docs](https://developers.circle.com/stablecoins/usdc-contract-addresses), from search, not read |

## 9. How this was checked (repeatable)

Endpoints: `https://polygon-bor-rpc.publicnode.com` (mainnet), `https://polygon-amoy-bor-rpc.publicnode.com` (Amoy),
`https://arbitrum-one-rpc.publicnode.com`. Tools: Foundry 1.5.1 (`cast`, `forge`), Python 3.

- **Token**: `cast call 0xc213…8e8F` with `name()`, `symbol()`, `decimals()`, `totalSupply()`, `implementation()`,
  `proxyOwner()`, `proxyType()`, `DOMAIN_SEPARATOR()`, `USDT0_VERSION()`, `oftContract()`, `getRoleMemberCount` and `getRoleMember`
  for each role. Safe: `getThreshold()`, `getOwners()`, `VERSION()`.
- **Domain separator**: `keccak256(abi.encode(keccak256("EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)"), keccak256("USDT0"), keccak256("1"), token, bytes32(137)))`, compared with `DOMAIN_SEPARATOR()`.
- **Source**: `https://sourcify.dev/server/v2/contract/<chain>/<address>?fields=sources,abi` for the implementation (137) and the
  composer (42161).
- **Quotes**: `quoteSend((uint32,bytes32,uint256,uint256,bytes,bytes,bytes),bool)` and `quoteOFT(...)` on the Polygon OFT
  `0x6BA1…9e13` and on Arbitrum's adapter `0x7765…9B92` and OFT `0x14E4…8D92`, with recipient `0x…dEaD` and 1,000,000,000 units.
  Two hops: `to` = the composer as bytes32, `extraOptions` = `0x0003 01 0023 03 0000 <gas u128 = 1> <value u128 = hop-2 fee>`,
  `composeMsg` = `abi.encode(SendParam)` for hop 2. Native drop: `0x0003 01 0031 02 <amount u128> <receiver bytes32>`.
  Credits: `credits(uint32)` on the adapter. Verifiers: the endpoint's `getSendLibrary`, `getReceiveLibrary`,
  `getConfig(oft, lib, 30110, 2)`.
- **Times**: `https://scan.layerzero-api.com/v1/messages/oapp/30109/0x6BA10300f0DC58B7a1e4c0e41f5daBb7D7829e13?limit=50`, then
  `/v1/messages/tx/<lzCompose tx hash>` for each two-hop message's second hop.
- **Secret check**: test values from a pure-Python secp256k1, checked with `t = 1`; then `eth_call` to `0x000…0001` with
  `0 ‖ v ‖ Gx ‖ s` on both networks; then a Foundry test of the Solidity check above, five cases, with `--gas-report`. One vector:
  `t = 0xd7ad6b53…ca16`, `s = 0xb5d75553…7610`, `address(T) = 0x73d8819b31105eebf456c261eaae5e02b04ad207`.
- **Chain timing**: average block time over 10,000 blocks from timestamps; `finalized` minus `latest`, 30 samples 2 s apart; `safe`
  block requested; consecutive timestamps over 40 blocks.
- **Bor rules**: `consensus/bor/bor.go` at tag v2.11.2 (`verifyHeader`, `Prepare`, `CalcProducerDelay`).
- **Gas**: `cast estimate --from 0x6ab707Aca953eDAeFBc4fD23bA73294241490620` (Aave's aPolUSDT, 9.5 M USDT0) for `transfer` and
  `approve`.

Added in the verification pass, the same day:

- **Verifier configs** for ten paths: `getConfig(oft, lib, eid, 2)` on the send and receive libraries for each EID.
- **Real sends**: `cast tx <hash> value` and `cast receipt <hash> gasUsed` for the source transactions in LayerZero Scan's feed.
  Inbound times from `/v1/messages/oapp/30110/<Arbitrum OFT>`. Native drops from the `destination.nativeDrop.status` of the
  last 100 messages of the Polygon, Arbitrum and Ethereum OFTs.
- **Permit signing**: a local `anvil` fork of Polygon mainnet. Anvil's public test key #0 (no real funds) signed permits with
  `@metamask/eth-sig-util` 8.2.0 and viem 2.21.55, and the permits were submitted to the real USDT0 contract on the fork.
- **Escrow gas**: the same fork, through `https://polygon.drpc.org` (publicnode refuses state older than a few minutes, which
  breaks a fork). A probe contract was deployed with `forge create`, and `approve`, `lock` and `claim` were sent with `cast send`,
  each as its own transaction.
- **Incidents**:
  - the largest gap between consecutive Bor timestamps in a three-day window, by recursive bisection against the window's median
    block rate (publicnode serves old headers);
  - blocks per hour;
  - Heimdall milestones by time, from `https://heimdall-api.polygon.technology/milestones/<n>`, comparing each milestone's
    timestamp with the Bor timestamp of its last block.
- **Spans**: `https://heimdall-api.polygon.technology/bor/params` and `/bor/spans/latest`.
- **Executor checks**: the verified sources of the endpoint (`EndpointV2.sol`) and of the OFT implementation, on Sourcify.

The scripts were throwaway, in the session's scratchpad. Each step above is a public call, or a local fork, that can be repeated.

## UNVERIFIED

Updated after the verification pass of 2026-11-22. Settled since the first draft: the Safe owners' kind, permit signing with
MetaMask's library, the incidents (measured on the chains), span length and Ithaca, escrow gas, verifier configs of every path,
Plasma's fee, the gas of an OFT send, permissionless execution, Scan's statuses, EIP-7702 on Polygon, TVM's `ecrecover`, a
rebalancing route, the Amoy faucets and the retired Amoy RPC.

Still open:

1. Who holds the five keys of USDT0's Safe `0x4DFF…0bf8`. They are plain keys, and USDT0 doesn't publish who holds them.
2. Whether real wallets (MetaMask's screen, Rabby, Trust over WalletConnect) accept and show a `permit` with Polygon's salt
   domain. MetaMask's signing library does.
3. Whether anything rotates a block producer that keeps producing while censoring; Ithaca's stall threshold.
4. What Bor's `finalized` returns during a milestone stall: stopped, or following Ethereum checkpoints.
5. Native-drop delivery into Polygon. Seen on USDT0 messages to Arbitrum and Ethereum only.
6. Which wallets let a page use EIP-7702 on Polygon, and which paymasters serve it.
7. Total time of an inbound transfer from Solana or Tron to Polygon through the composer. None was found; hop 2 alone takes
   15–22 minutes.
8. What `APPLICATION_BURNED` means on LayerZero Scan; what happens to a mesh message cancelled by `burnAndNilifyAdmin`; what
   USDT0 support does for a stuck message (carried from sol2tron).
9. One WalletConnect session carrying both `solana` and `eip155`.
10. That TVM's `ecrecover` accepts this check's input, and a usable secp256k1 recovery on TON (inventory alternative only).
11. Bitfinex's current fees for USDt on Liquid and USDT0 on Polygon, from Bitfinex itself; other rebalancing routes.
12. That Base has no Tether-issued USDT (inferred from its absence in USDT0's list, not from Tether).
13. That the Amoy faucets work for a fresh address.
