# liq2any: Phase 1 design

Written 2026-11-22. **The user accepted every recommended option in section 9 on 2026-11-22.** The other options stay listed as a
record of the trade-offs.

Facts here are sourced in [assumptions.md](assumptions.md), cited by section (§). liq2sol is at `~/src/liq2sol`; its documents
are cited as `liq2sol/docs/<file>`. Where this design repeats liq2sol, it says so and doesn't restate the reasoning.

Section 9 lists the decisions, each with its options. Everything that depends on one of them says so.

## 1. What is being built

1. **A Polygon escrow contract** for USDT0, the counterpart of liq2sol's Solana escrow (section 2).
2. **Polygon in the maker**: a chain module, signer rules, inventory and pricing (section 4).
3. **RFQ version 3**, in which directions become pairs of chains (section 5).
4. **The page**: routes to other chains, saying plainly which legs are atomic and which trust a bridge, and recovery when a
   bridge leg is slow or stuck (section 6).

The Liquid side doesn't change. The MuSig2 taproot output, adapter-signed claims, blinding, the Simplicity refund leaf, the
signing sessions and the early refunds are all reused from liq2sol as they are (`liq2sol/docs/confidential.md`,
`liq2sol/docs/protocol.md`). The one addition on the Liquid side is a function that turns the point `T` into its Polygon form,
`address(T)` (§2.2).

**Test networks first (user's decision, 2026-11-22).** Development and the first releases run on Liquid testnet and Polygon Amoy,
with a stand-in token on Amoy, because USDT0 has no testnet (§8). Bridge routes can't run on testnets, so they come later, on
mainnet (section 6.5).

### Trust, as the page must say it

- **The swap between Liquid and Polygon is atomic and trustless**, as in liq2sol. Neither party can take the other's funds, and
  either can get its own back from the chains alone. It trusts what liq2sol trusts, plus Polygon's consensus and its block
  producer, and Tether's power over USDT0 on Polygon (a 3-of-5 Safe can block addresses and burn their funds, §1.2).
- **A leg to or from any other chain goes through USDT0's bridge, and trusts it**:
  - Tether's USDT0 contracts, which the same Safe can upgrade;
  - three verifiers, LayerZero Labs, USDT0 and Canary, all of which must sign (§5.2);
  - for Solana, Tron, TON and Celo, also the Legacy Mesh's pool and its admins (§5.1).

  The maker never touches that leg, so the maker stays non-custodial. The taker's own wallet sends it, and the taker chooses
  whether to.

## 2. The Polygon escrow

### 2.1 Shape

One immutable contract, `Escrow`, per network (decision P3). It names a single token, set in the constructor: USDT0 on
mainnet, the stand-in on Amoy. It has no owner, no upgrade, no pause and no fee.

**Where the coins sit (decision P7, accepted).** **Each swap's coins sit at their own address**, a minimal vault contract
deployed by `Escrow` with CREATE2 for that swap. The vault's only function lets `Escrow` pay out of it.

- Tether's blocklist stops a blocked address from sending (§1.2). If every swap's coins sat in `Escrow` itself, one block would
  freeze all open swaps, and `destroyBlockedFunds` could burn them.
- With a vault per swap, a block on `Escrow` stops new locks only: a vault sends its coins itself, and `Escrow` is only its
  caller. A block on one vault stops one swap.
- This is what liq2sol's Solana escrow does with a token account per swap (`liq2sol/docs/protocol.md` section 7.4).
- It costs a minimal-proxy deployment per lock, roughly 45k more gas, under $0.004 at today's prices (§4). To be measured in
  Phase 2.

### 2.2 The secret

As liq2sol: the taker makes `t`, with `T = t·G`. The escrow stores `lockAddress = address(T)`, the last 20 bytes of
`keccak256(T.x ‖ T.y)`, and a claim reveals `s = t·Gx mod n`. The check, measured at 3,736 gas (§2.2):

```solidity
lockAddress != address(0) && ecrecover(bytes32(0), 27, GX, s) == lockAddress
```

- `lock` refuses `lockAddress == 0`, so `s = 0` (whose recovery fails to `address(0)`) can never open anything.
- `v` is always 27.
- The claim **stores `s` in the escrow's storage**, as liq2sol stores `revealed`. A counterparty reads it with one `eth_call`
  and never needs logs, which free RPCs limit.
- `T` itself never appears on Polygon before the claim, only its hash (§2.2).
- New shared code: `swapkey::polygon_address_of(T)`. Its test vectors come from three independent computations: the pure-Python
  curve used in Phase 0, the Rust code, and the contract on a fork. One of them, `t = 1`, gives `0x7e5f…5bdf`.

### 2.3 Storage

Escrow id `id = keccak256(abi.encode(initiator, lockAddress))`. The initiator is part of the id, so nobody can occupy another
party's id by locking first with the same point, as liq2sol's seeds include the initiator.

| Field | Type | Notes |
|---|---|---|
| `initiator` | address | Locked the coins, and receives a refund or release |
| `recipient` | address | Receives the claim. Not zero, not the initiator. |
| `lockAddress` | address | `address(T)`. Not zero. |
| `amount` | uint128 | Base units, more than 0 |
| `deadlineTime` | uint64 | Seconds |
| `deadlineBlock` | uint64 | Block number |
| `longLeg` | bool | True for the taker's lock (P→L), false for the maker's (L→P). Sets the clock rule below. |
| `extended` | bool | `extend` has been used |
| `state` | enum | `None`, `Open`, `Claimed`, `Refunded` |
| `revealed` | bytes32 | `s`, set by the claim |
| `gasDrop` | uint96 | Native POL forwarded at lock (decision P2). Recorded for the record only. |

### 2.4 The clock rule

Phase 0 found that Polygon's two clocks fail in opposite ways (§3.3, §3.4):

- **block numbers** stop in a halt, but are safe against a producer who runs `block.timestamp` slow;
- **timestamps** jump to real time after a halt, but a producer can run them slow (at worst +1 s a block).

So every escrow carries both deadlines, and combines them by leg (decision P8):

```text
short leg (maker's L→P lock):  can_claim = Open && time < deadlineTime && block < deadlineBlock
long leg  (taker's P→L lock):  can_claim = Open && (time < deadlineTime || block < deadlineBlock)
                               can_refund = Open && !can_claim
```

- **Short leg, `AND`:** whichever clock is the honest one closes the taker's claim on time. After a halt the timestamp has
  jumped. Against a slow clock, the block count holds. This is the Polygon version of liq2sol's `close_on_restart`, with no
  sysvar needed.
- **Long leg, `OR`:** the maker's claim stays open until both have passed. A halt doesn't eat the maker's margin, because blocks
  stop too. A change in block time doesn't shorten the leg, because the timestamp still runs.
- The two deadlines are set from the same wall-clock target. The block one is computed from the measured block rate (1.5 s on
  mainnet, 1.0 s on Amoy today, §3.1), never from a constant.
- `can_claim` and `can_refund` are exact opposites while open (liq2sol invariant S4).

### 2.5 Functions

| Function | Caller | Checks | Effect |
|---|---|---|---|
| `lock(lockAddress, recipient, amount, deadlineTime, deadlineBlock, longLeg, gasTo)` payable | Anyone, as initiator | Fields as in 2.3; both deadlines in the future and at most `MAX_WINDOW` (14 days) ahead; id unused | Creates the vault, pulls `amount` with `transferFrom`, and **checks the vault received exactly `amount`**. Forwards `msg.value` to `gasTo` at once (decision P2). |
| `lockWithPermit(…, permitDeadline, v, r, s)` | Same | As `lock`. The permit is tried and its failure ignored, as long as the allowance is enough afterwards, so a front-run permit can't block the lock. | One transaction instead of approve and lock (§1.3) |
| `claim(id, s)` | **Anyone** | `can_claim`; the secret check | Stores `s`, state `Claimed`, pays `recipient` from the vault |
| `refund(id)` | **Anyone** | `can_refund` | State `Refunded`, pays `initiator` |
| `release(id)` | `recipient` only | Open | Pays `initiator` at once. The early refund, as liq2sol's `release`. |
| `extend(id, newTime, newBlock)` | `recipient` only | Long leg, not extended yet, open and claimable; each new deadline later than the old and at most 7 days (or the block equivalent) further | Moves both deadlines, once. liq2sol's D1, for a Liquid halt in P→L. |

Everything pays only to addresses fixed at lock time, so `claim` and `refund` are permissionless, as on Solana. That lets watchtowers
and relayers act with no trust (liq2sol `protocol.md` section 10).

**Not in the contract:** an on-chain cap (decision P4), a fee, an owner, and any call to a bridge. Bridge legs are sent by the
taker's own wallet. If the escrow sent one, a failed second hop would be refunded to the escrow (§5.1).

### 2.6 Maker vault (decision P9)

liq2sol's maker vault (its D6) lets a hot operator key lock up to a cap per window, while a cold owner key alone can withdraw. The
same can be added here as a separate contract, `MakerVault`, which calls `Escrow.lock` with `initiator` set to the vault. It
brings back one blockable address for the maker's float, which is the maker's own risk to take. Accepted (P9) for mainnet, not
needed for the first testnet release.

### 2.7 Tests

Foundry, in `contracts/polygon/`, with these suites:

- **The secret check:** the right `s`; a wrong one; `s = 0`; `s = n`; `s` above n; `v = 28` (`−T`); a zero `lockAddress`; the
  shared vectors.
- **The clock rule:** an exhaustive grid over both bounds and both legs, as liq2sol's `exactly_one_of_claim_or_refund_while_open`.
  Halts are simulated by warping time with no blocks, and slow clocks by blocks with no time.
- **Each function:**
  - claim twice;
  - claim after the deadline, on each clock;
  - refund early;
  - release by someone other than the recipient, and release after a settle;
  - extend on a short leg, twice, too far, after the deadline, by the initiator;
  - lock with a used id;
  - lock with a self-recipient;
  - lock with zero amount;
  - lock with a far deadline.
- **Adversarial tokens:** a fee-on-transfer stand-in (refused by the exact-amount check), a token whose transfer returns false, and
  one that reenters.
- **`lockWithPermit`:** after someone front-runs the permit; with an expired permit; with too little allowance.
- **Front-running:** a lock by someone else with the same `lockAddress` gets a different id and can't affect ours.
- **The real token, on a fork of Polygon mainnet:**
  - claim and refund with real USDT0;
  - Tether's Safe (impersonated on the fork) blocks a vault, `Escrow` itself, the recipient, or the initiator, and the test checks
    what still works;
  - `destroyBlockedFunds` on a vault.
- **Invariants (fuzzed):**
  - tokens are conserved;
  - a settled escrow never moves again;
  - `can_claim` and `can_refund` are exact opposites;
  - only `recipient` or `initiator` ever receives.
- **Mutation checks:** remove each rule in turn and see a test fail, as `liq2sol/scripts/mutate-confidential.py` does.
- **Gas** of each function measured and recorded, and Amoy deployment checked against the build.

## 3. The protocol

### 3.1 Units and amounts

Quotes are in USDT0 base units (6 decimals), as liq2sol quotes in SOL-USDT units. The Liquid amount is that times 100, and must be
exact. Each side receives exactly what it agreed to, with the spread in the difference, as in liq2sol.

### 3.2 Liquid → Polygon (L→P)

The same as liq2sol's L→S, with Polygon in Solana's place (`liq2sol/docs/protocol.md` section 4):

1. RFQ. The taker's accept carries `T`, its per-swap Liquid key, the blinding key, its **Polygon address** (the recipient), and
   where the gas drop goes (decision P2).
2. The taker pays the confidential Liquid address.
3. The maker unblinds the coin and checks asset, amount and confirmations, then runs the signing session, and stores the adapter
   signature durably.
4. The maker locks on Polygon: `longLeg = false`, recipient the taker, with the gas drop.
5. The taker checks the escrow at `finalized` on two independent endpoints, simulates the claim with `eth_call`, and checks the
   cutoff. Then it claims, revealing `s`.
6. The maker reads `s` from the escrow (or from the mempool, earlier), computes `t`, and claims on Liquid.

**About step 5.** EVM wallets can't sign a transaction without sending it (§6), so unlike liq2sol the page can't hold the signed
claim and choose its moment. The protections that matter remain:

- the two-endpoint check and the simulation come before the wallet is asked;
- the claim is permissionless, so the page can rebroadcast the same `claim(id, s)` through its own endpoints, or through anyone, if
  the wallet's one stalls.

### 3.3 Polygon → Liquid (P→L)

The same as liq2sol's S→L (`liq2sol/docs/protocol.md` section 5):

1. RFQ, with the taker's Liquid receiving script.
2. The taker locks on Polygon: `longLeg = true`, recipient the maker. This is `approve` and `lock`, or `lockWithPermit`.
3. The maker checks the escrow at `finalized`: every field, the full `lockAddress`, `longLeg = true`, and both deadlines against
   its gate. Then it funds the Liquid address, with a blinded L-BTC fee coin for the taker.
4. The signing session, in liq2sol's order: the maker stores the adapter signature before sending its share.
5. The taker completes the signature with `t` and claims on Liquid.
6. The maker extracts `t`, computes `s`, and claims on Polygon.

### 3.4 Timeouts (decision P8)

Proposed values, from Phase 0's measurements. **Testnets would use the same wall-clock values.**

- One span of Polygon's single block producer is about 2.7 hours (§3.3). A producer could censor a claim for that long.
- So in L→P the taker's claim window on Polygon is **longer than one span**. Its claim is the reveal, so a censored claim lets the
  maker learn `s` from the mempool while the taker's payment waits.
- In P→L, the maker, which must claim on Polygon after the taker's reveal on Liquid, gets **at least two spans** of margin.

**L→P** (taker's Liquid lock long, maker's Polygon lock short):

| Parameter | Value | Wall time |
|---|---|---|
| `LOCK_HEIGHT` | `h_accept + 480` | about 8 h |
| Polygon deadlines | maker's lock + 3 h, in seconds and in blocks | 3 h: longer than one span |
| Maker lock gate | Lock only if `LOCK_HEIGHT − tip ≥ 390` | Leaves 3 h of escrow plus 3.5 h for the maker's Liquid claim |
| Taker claim cutoff | Don't reveal if less than 30 min remains on either Polygon clock | |
| Worst case for the taker | About 8 h, if the maker never locks | liq2sol: 5 h |

**P→L** (taker's Polygon lock long, maker's Liquid lock short):

| Parameter | Value | Wall time |
|---|---|---|
| `LOCK_HEIGHT` | `h_accept + 90` | about 90 min, as liq2sol |
| Polygon deadlines | taker's lock + 8 h, in seconds and in blocks | 8 h |
| Maker lock gate | `tip ≤ h_accept + 30`, and at least 6.5 h left on both clocks | |
| Maker's margin after `LOCK_HEIGHT` | At least about 5.5 h | Two spans |
| Taker claim cutoff | Don't reveal if `LOCK_HEIGHT − tip < 10` | As liq2sol |
| Worst case for the taker | About 8 h if the maker never locks, plus up to 7 days if it uses `extend` | liq2sol: 6 h |

These margins **don't** cover Polygon's finality stalls of 5–11 hours (§3.2). A stall doesn't threaten funds, because blocks go
on, both clocks run, and every party refuses to act on what isn't finalized:

- In L→P, a taker that can't see the maker's lock finalized doesn't reveal. It refunds on Liquid, and the maker on Polygon.
- In P→L, a maker that can't see the taker's lock finalized doesn't fund.

What a stall does do is turn swaps in flight into refunds.

**The one stall case that needs care** is a reorganisation of blocks that weren't finalized, as nodes saw in September 2025
(§3.2). Each side watches its own Polygon claim until it is finalized, and rebroadcasts it if it drops, which the margins above
leave room for.

### 3.5 Failure behaviour

liq2sol's tables carry over (`liq2sol/docs/protocol.md` section 7), with Polygon in Solana's place:

| Event | Effect | Handling |
|---|---|---|
| Liquid halt, L→P | `LOCK_HEIGHT` can't pass. The maker claims when Liquid resumes. | Safe, delayed |
| **Liquid halt, P→L, both locked** | Polygon's clocks run on. | `extend` by the maker, once, up to 7 days (liq2sol's D1) |
| **Polygon halt, L→P, both locked** | Liquid's clock runs on. | The short leg's `AND` rule: the timestamp jumps after the halt and closes the taker's claim. Residual: a halt plus a producer that also runs the clock slow. |
| Polygon halt, P→L | Blocks stop, so the long leg's `OR` rule keeps the maker's claim open. | Safe, delayed |
| Polygon finality stall | Blocks go on. Nobody acts on unfinalized state. | Swaps in flight end in refunds. See 3.4. |
| Censoring producer | A claim is held back for up to a span | Margins of two spans (3.4). Residual: a producer that keeps its place for longer. |
| Tether blocks the taker's address, L→P | The claim still pays it (`to` isn't checked), and the coins are then stuck there | The page checks `isBlocked(recipient)` before locking, and refuses. The maker's signer checks it too. |
| Tether blocks the maker's address, P→L | Same: the claim still pays it, but the maker can't move the coins | Maker's own risk, checked at the gate |
| Tether blocks a vault | That swap is stuck until unblocked, or its coins are burned | One swap only (2.1) |
| Wrong amount or asset on Liquid | As liq2sol | Refund only |

### 3.6 Early refunds and cancelling

As liq2sol (`liq2sol/docs/protocol.md`, "Early refund" and "Cancelling"):

- L→P: early refund on Liquid before the maker locks on Polygon;
- P→L: `release` before the maker locks on Liquid;
- `cancel-ask` before anything is locked.

The signer's "never lock" promise carries over unchanged.

## 4. The maker

### 4.1 Components

liq2sol's daemon (`liq2sol/docs/maker.md`) gets a `chain_polygon` module beside `chain_solana`. Neither replaces the other if the
two routes merge (decision P6).

| Module | Change |
|---|---|
| `chain_polygon` | JSON-RPC to at least two independent providers. Decisions use `finalized` only. Reads escrows by id with `eth_call`, so it needs no logs or indexer. Checks `finalized` against Heimdall's latest milestone. For mainnet, an own Bor node is better but heavy; two providers are the minimum. |
| `engine` | L→P and P→L state machines, which are liq2sol's L→S and S→L with Polygon's clock rule and checks |
| `signer` | Polygon rules, 4.2 |
| `rfq` | Version 3, section 5 |
| `monitor` | Polygon halt and stall detection, 4.4 |

A Rust EVM library is needed for transactions and ABI encoding. Recommended: `alloy`, the successor of `ethers-rs`, pinned and
vetted like the rest. It is the largest new dependency.

### 4.2 Keys and what the signer refuses

New keys:

| Key | Use | Where |
|---|---|---|
| Polygon operator key | Locks (L→P), claims (P→L), `extend`, `release`, gas drops. Holds a little POL and, without a vault, the USDT0 float. | Hot, in the signer |
| Vault owner (if P9) | Withdraws, changes the operator | **Cold**, never on the host |

The signer builds every Polygon transaction itself, as it does on Solana and Liquid: the chain id is pinned, the call is fixed and
the destination is the network's own `Escrow`. It reads the chains through its own connections. It refuses:

- an L→P lock without the taker's exact Liquid coin confirmed and an adapter signature stored (as L→S);
- a lock to a `lockAddress` it has seen before;
- a lock whose recipient Tether has blocked, or whose deadlines break the gate;
- a lock whose gas drop is above the configured cap;
- a P→L Liquid lock without the taker's escrow at `finalized` with exactly the agreed fields, including `longLeg = true`;
- `release` after it has signed a Liquid lock for the swap, and a Liquid lock after it has signed a `release` (liq2sol's rule);
- `extend` unless the Liquid tip is older than its halt threshold;
- anything over the per-swap and per-window caps.

**EVM nonces, the new failure mode.** An account's transactions must use consecutive nonces, and a stuck one blocks all later
ones. The signer records, durably and before signing, which call each nonce carries. It may re-sign the same call at the same
nonce with a higher fee, to unstick it, and never signs a different call at a used nonce. This is the EVM form of liq2sol's rule
that a retried lock sends the same bytes.

### 4.3 Inventory, pricing, rebalancing

- Two balances: L-USDt on Liquid and USDT0 on Polygon, plus small L-BTC and POL for fees and gas drops.
- `available_l2p` and `available_p2l` are published as liq2sol's `available_*`.
- Prices per pair and per size step, as liq2sol. The quote covers the gas drop and Polygon gas, which are cents.
- **Rebalancing stays manual** (liq2sol's D9). A maker moves coins between Liquid and Polygon itself, for example through Bitfinex,
  which handles both (§7), and the daemon only alerts. The daemon never holds an exchange key.

### 4.4 Halt and stall detection

The maker stops new swaps when any of these holds:

- the Liquid tip is older than 10 minutes (as liq2sol);
- Polygon's `finalized` block hasn't moved for 60 s;
- `finalized` trails the head by more than 100 blocks (about 2.5 minutes), which catches a milestone stall while blocks go on.

During a Liquid stall it `extend`s open P→L escrows, as liq2sol does for S→L.

### 4.5 Exposure caps

As liq2sol, per direction, with defaults left until testnet. Added: total open L→P (a Polygon halt plus a slow clock), and total
open P→L (a Liquid halt beyond `extend`, censorship).

## 5. RFQ version 3

Same transport as liq2sol (`liq2sol/docs/nostr-rfq.md`): offers in kind `38401`, private messages in ephemeral gift wraps
(`21059`), and the same replay and spoofing rules. What changes is that a direction becomes a pair of chains.

**Offer** (`d` tag `liq2any-v3-<network>`, or shared with liq2sol under decision P6):

```json
{
  "v": 3,
  "network": "testnet",
  "chains": {
    "liquid":  { "asset": "<hex32>", "confirmations": 2, "fee_coin_sats": 30 },
    "polygon": { "chain_id": 80002, "escrow": "0x…", "token": "0x…", "max_gas_drop": "200000000000000000" }
  },
  "pairs": [
    { "from": "liquid", "to": "polygon", "min": "10000000", "max": "1000000000", "available": "500000000",
      "prices": [{ "up_to": "1000000000", "bps": 30 }] },
    { "from": "polygon", "to": "liquid", "min": "10000000", "max": "1000000000", "available": "750000000",
      "prices": [{ "up_to": "1000000000", "bps": 40 }] }
  ],
  "relays": ["wss://relay.primal.net", "wss://relay.coinos.pro"]
}
```

- A pair names chains, not routes. **Bridge legs aren't in the RFQ at all**: the maker never sees them, and the page plans them.
- Amounts are 6-decimal base units, as strings. Prices, steps and `available` keep liq2sol's meaning.
- If liq2sol merges in (decision P6), Solana becomes one more entry under `chains`, `{ "program_id": … }`, with pairs
  `liquid`↔`solana`, and liq2sol's current offer becomes the v3 form of what it already says.

**Private messages** keep liq2sol's types and order. The `body` fields that name a chain become per-chain:

- `rfq-request`: `{ "from": "liquid", "to": "polygon", "amount_side": "send", "amount": "…" }`.
- `rfq-accept`:
  - `lock_point`, `taker_key` and `blinding_key` as before;
  - `taker_polygon` (L→P: the recipient; P→L: the initiator the maker will look for);
  - `gas_to` and `gas_drop` (L→P, decision P2).
- `rfq-terms`:
  - `lock_height`, `maker_key` and `liquid_amount` as before;
  - `polygon_amount`, `maker_polygon`, `escrow`, `token` and `chain_id`, which the page checks against its pinned values;
  - `deadline_time` and `deadline_block` (L→P: what the maker will lock with; P→L: the minimum the maker accepts);
  - `gas_drop`.

The signing session is unchanged. Every check in liq2sol's section 4 applies to the new fields, plus one: the page refuses terms
whose `escrow`, `token` or `chain_id` differ from its pinned values for the network.

## 6. The page

### 6.1 Routes

The taker says what it has and what it wants. Liquid USDt is always one end; the other is USDT on some chain. The page builds the
route from up to three legs:

```text
[ inbound bridge to Polygon ]  →  atomic swap, Polygon ↔ Liquid  →  [ outbound bridge from Polygon ]
```

| Taker wants | Legs | Bridge fee and time today (§5.3) |
|---|---|---|
| Liquid ↔ Polygon | Atomic swap only | None |
| Liquid → Arbitrum, Ethereum, Optimism, Plasma, Ink, Mantle, Monad, Unichain, HyperEVM… | Atomic, then 1 hop | $0.00–$0.09 plus about $0.01 of gas, about 1 minute (Ethereum up to 2) |
| Liquid → Solana | Atomic, then 2 hops via Arbitrum | $0.67 plus 0.03%, about 72 s. **liq2sol does this atomically**, so the page offers that first when a liq2sol maker can fill it (decision P6). |
| Liquid → Tron | Atomic, then 2 hops | **$13.60** plus 0.03%, about 107 s |
| Liquid → TON, Celo | Atomic, then 2 hops | $0.14 or $0.34, plus 0.03% |
| Any of those → Liquid | 1 or 2 hops into Polygon, then atomic | The source chain's fee, and **15–22 minutes** for anything through Arbitrum (§5.3) |
| Base | Not offered | Base is not a USDT0 chain (§5.1) |

### 6.2 Saying what is atomic, before the taker commits

Before anything is signed, the page shows each leg as its own step, with its own label:

- **Atomic leg:** "Swap with a maker. If anything goes wrong, you get your money back from the chains alone."
- **Bridge leg:** "Sent through USDT0's bridge. This trusts Tether's USDT0 contracts and the three verifiers that approve every
  transfer. Once sent it can't be cancelled, and if it gets stuck only Tether's operators can release it. The maker isn't
  involved."
  - Plus, for two-hop routes: "Also goes through the Legacy Mesh pool on Arbitrum, which takes 0.03%."

The confirmation screen lists every leg with its cost in dollars and its expected time, and makes the taker tick that it has read
the bridge warning. **Tron's fee is shown in red with the amount**, as sol2tron does.

The taker may stop after the atomic leg: the USDT0 is then in its own Polygon wallet, and the bridge step can be sent later, or
never.

### 6.3 Sending a bridge leg

- From the taker's own EVM wallet, after the atomic leg has paid it. Never from the escrow (§5.1).
- **Outbound needs only an address on the destination chain**, no wallet there. The page validates it:
  - an EVM address with a checksum;
  - a Solana wallet address, refusing token accounts, as sol2base and sol2tron do;
  - a Tron base58 address, decoded to its 20 bytes.
- The page quotes each leg live before signing:
  - `quoteSend`, for one hop and for two (with hop 2 priced on Arbitrum first, §9);
  - and for two-hop routes, reads the mesh's `credits` for the destination, refusing an amount above it.
- **Inbound** (P→L from another chain) needs a wallet on the source chain. That is EVM wallets for USDT0 chains, and sol2tron's
  Solana and Tron code for those. It is planned after outbound.

### 6.4 When a bridge leg is slow or stuck

The page records each bridge leg in the swap's recovery file: source transaction, LayerZero guid, route and destination. Then it
tracks it two ways:

- through LayerZero Scan's API (no key, CORS open, §5.4), by transaction hash;
- on-chain, by the destination OFT's `OFTReceived(guid, …)` event, so it doesn't depend on Scan alone.

What it shows and offers, by status:

| Status | Meaning | The page |
|---|---|---|
| `INFLIGHT` past twice the expected time | Waiting for confirmations or verifiers | Shows the stage from Scan's sub-statuses. Nothing to do but wait. |
| Verified, not executed | The executor hasn't delivered | **Offers to execute it**: anyone may (§5.4). This needs gas on the destination chain, which the page says. |
| `FAILED` or `PAYLOAD_STORED` | Delivery reverted | Offers a retry with more gas, through the endpoint |
| Two-hop: hop 2 failed on Arbitrum (the composer's `SendFailed` or `DecodeFailed`) | The coins wait in the composer | Offers the composer's `refund(guid)`, which returns them to the taker's Polygon address, or `retry(guid)`. Both are permissionless, and cost ETH on Arbitrum. |
| `BLOCKED`, or a verifier never signs | Only USDT0's operators can help | Says so, with links, the guid and USDT0's contact |

The page never offers anything that could move funds anywhere but the original recipient or sender, because the contracts allow
nothing else.

### 6.5 Test networks

- Amoy with the stand-in token: the atomic swap only.
- Bridge routes appear only on mainnet. They can be developed against our own test OFTs on two testnets, or against mainnet with
  small amounts once the user agrees (§8).
- POL for testnet takers comes from the test maker's gas drop, which also tests decision P2.

### 6.6 Wallets and code

- **EVM:** injected wallets through EIP-6963, else WalletConnect.
  - The `eip155` namespace (`eip155:137`, `eip155:80002`) goes into liq2sol's existing `SignClient`, one client per page.
  - Methods: `eth_sendTransaction` and `eth_signTypedData_v4` (for `lockWithPermit`).
  - Encoding and decoding in the WASM, from the same shared Rust the maker uses, with tests comparing the page's transactions with
    the maker's, as liq2sol does for Solana. No viem (user's decision, 2026-11-23).
- **Reads:** two independent endpoints for every decision. publicnode and drpc both send open CORS headers, on mainnet and on Amoy
  (checked 2026-11-22). Heimdall's milestone API also sends them, so the page can see a stall itself.
- **Before revealing `s`:** the escrow at `finalized` on both endpoints, `isBlocked(recipient)` false, and an `eth_call`
  simulation of the claim.
- **Licences:** the page stays in `client/` under AGPL-3.0-only. The contract, maker and shared code are MIT.

## 7. What stays from liq2sol

Unchanged, and to be copied rather than rewritten:

- the Liquid construction and its tests;
- the signer's process separation and its Liquid rules;
- the recovery file;
- the vetted-maker list and favourites;
- logs that scrub secrets;
- the release, signing, reproducible-build and nsite tooling;
- the shared menu and footer.

Every "easy to get wrong" point in liq2sol's `CLAUDE.md` about Liquid still applies.

## 8. Build order, once accepted

1. Contract and its tests on Foundry, then on a mainnet fork, then deployed to Amoy.
2. `swapkey::polygon_address_of`, its vectors, and the cross-check against the contract.
3. Maker: `chain_polygon`, signer rules, engine, RFQ v3; the daemon's failure scenarios on Amoy and Liquid testnet.
4. Page: the atomic swap on testnets; browser tests as liq2sol's.
5. Mainnet: contract deployed by the user, then outbound bridge legs, then inbound.

## 9. Decisions (accepted 2026-11-22)

### P1. Which target chains first

| Option | Trade-off |
|---|---|
| **Polygon only on testnets; then outbound to one-hop USDT0 chains (Arbitrum, Ethereum, Optimism, Plasma); then Solana and Tron via two hops; inbound last (accepted)** | Each step adds the least new code. One-hop legs are cents and a minute. Outbound needs no wallet on the destination. Solana is already atomic through liq2sol. |
| All outbound routes at once | Same code for all EVM chains, plus two-hop handling. More to test at once. |
| Inbound early | Needed for P→L from other chains. Needs source-chain wallets (sol2tron's code for Solana and Tron), and 15–22 minutes of waiting. |

### P2. Taker gas on Polygon

| Option | Trade-off |
|---|---|
| **The maker's lock forwards a little POL to the taker (`gasTo`), sized for the claim, and optionally for the onward bridge fee; priced into the quote (accepted, for L→P)** | No POL needed up front. A few lines in `lock`. The maker loses the drop if the taker never claims, so the drop is capped (`max_gas_drop`), and the taker has locked Liquid funds first, which limits griefing. |
| The taker must hold POL | No contract support. A Liquid user usually has none. |
| Permissionless claim with a tip for whoever sends it | No POL needed. The taker must hand `s` to a relayer before the claim lands, and loses the swap if nobody sends it in time (liq2sol rejected the maker variant as D5). |
| For P→L: a signed lock order sent by the maker | No POL needed to lock. Signature checks in the contract, and the maker chooses when to send it until the order expires. |
| For P→L: nothing, since a taker with USDT0 on Polygon nearly always has POL, and one bridging in gets a native drop (§4) (**accepted, for P→L**) | No contract support. The page adds the native-drop option to inbound bridge legs. |

### P3. An immutable contract

| Option | Trade-off |
|---|---|
| **Immutable on mainnet; redeployed freely on testnets (accepted, as liq2sol's D8)** | Nobody needs to trust an upgrade key. A bug means a new address and a page release. |
| Upgradeable behind a timelock or multisig | Bugs can be patched. Adds a trusted party, which the project avoids. |

### P4. The cap

| Option | Trade-off |
|---|---|
| **The page's and the maker's cap, as liq2sol's 1000 USDT a swap, raised only by a release (accepted)** | Same rule as liq2sol. The contract stays general. |
| Also an on-chain maximum in the contract | A bug in the page or the maker can't exceed it. Raising it means a new immutable contract. |

### P5. Name and repository

`SwapMarket/liq2any` now exists and is public (2026-11-22).

| Option | Trade-off |
|---|---|
| **Keep liq2any (accepted)** | Already in place. Says what it does. |
| Rename (for example liq2poly) | More exact for the core, less for the routes. GitHub redirects renamed repositories. |

### P6. liq2sol: separate or merged

| Option | Trade-off |
|---|---|
| **Separate repositories and daemons for now, with one shared RFQ v3 and a page that can use both (accepted)** | Nothing live changes. The page can prefer liq2sol's atomic Solana route and fall back to the bridge. liq2sol moves to v3 messages when convenient. |
| Merge into one daemon and one page | One codebase and one place for makers. A large refactor of a live mainnet system. |
| Fully separate, no shared RFQ | Least work. Two books, and Solana routes compete instead of complementing each other. |

### P7. Where escrowed coins sit

| Option | Trade-off |
|---|---|
| **A vault per swap (accepted)** | A Tether block hits one swap, or only new locks. About 45k more gas per lock (cents). A little more code. |
| All in the escrow contract | Simplest and cheapest. One block freezes every open swap, and `destroyBlockedFunds` could burn them all. |

### P8. Deadlines

| Option | Trade-off |
|---|---|
| **Both clocks on every escrow, combined by leg (2.4), with the durations of 3.4 (accepted)** | Closes the Polygon-halt theft without a restart signal, and keeps a long leg's margin through a halt or a block-time change. Longer taker waits when a maker never locks: about 8 h, against liq2sol's 5–6 h. |
| Shorter durations (L→P short leg 1 h, P→L 6 h) | Faster refunds. Less than a span of margin against a censoring producer. |
| Timestamps only | Simpler. The long leg loses its margin in a Polygon halt (§3.4). |

### P9. Maker vault on Polygon

| Option | Trade-off |
|---|---|
| **Port it for mainnet, not for the first testnet release (accepted, as liq2sol's D6)** | Caps what a stolen hot key can lock per window. One more contract and its tests. |
| Never | The hot key holds the whole float. |

### P10. Who sends the onward bridge leg

| Option | Trade-off |
|---|---|
| **The taker's wallet, after the swap has paid it (accepted)** | Funds are never in a key the page holds, as the user decided for liq2sol's wallets on 2026-11-05. One more wallet prompt. |
| A per-swap key the page holds, kept in the recovery file, which claims and bridges with no wallet prompt | Smoothest: the taker gives only a destination address. But the swap's coins sit in a browser key between claim and bridge, and a lost recovery file then loses them. |
