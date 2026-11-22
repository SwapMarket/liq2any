# liq2any: the brief

Swap USDt on Liquid with USDT on other chains, trustlessly where it can be, from a static page, with makers quoting over Nostr.
It grows out of liq2sol (`~/src/liq2sol`, live on mainnet since 2026-11-17), which swaps Liquid USDt with Solana USDT.

This brief was written on 2026-11-22, at the end of a long conversation in `~/src/liq2sol`. It is a starting point, not a design.
Treat every claim below that isn't in liq2sol's own docs as **UNVERIFIED** until a phase checks it and cites a source.

## The idea (user's decision, 2026-11-22)

- **The maker holds only two things: USDt on Liquid, and USDT0 on Polygon PoS.** No inventory on any other chain.
- **The atomic, trustless core is Liquid ↔ Polygon**, built the way liq2sol does Liquid ↔ Solana: a confidential MuSig2 output on
  Liquid with an adapter-signed claim, and an escrow on the other chain that pays out to whoever reveals the swap's secret. Here that
  escrow is a new contract on Polygon, for USDT0.
- **Every other chain is reached through routes already researched or built**, by the taker's own wallet, before or after the
  atomic swap:
  - **USDT0's own bridge** (LayerZero OFT) between USDT0 chains. USDT0's pages say it takes no protocol fee there; the sender pays gas
    and a LayerZero messaging fee.
  - **USDT0's Legacy Mesh** for Solana, Tron, TON, Celo and Ethereum's legacy USDT: 0.03% plus the messaging fee. Researched in
    detail for Solana ↔ Tron in `~/src/sol2tron/docs/assumptions.md`.
  - **Circle's CCTP**, for USDC (sol2base, eth2base), if USDC is ever in scope.
  - liq2sol itself for Liquid ↔ Solana, which stays as it is.
- **Not Alchemy.** Its cross-chain swaps are an alpha inside its embedded-wallet APIs, with an API key, and it doesn't say which
  bridges it routes through or what it charges. It adds a dependency and a margin over calling USDT0's bridge directly.

**What this means for trust, which the design must say plainly to the user:** only the Liquid ↔ Polygon swap is atomic and
trustless. A leg through USDT0's bridge trusts that bridge: Tether's contracts, their admins, and LayerZero's verifiers. The maker
never handles that leg, so the maker stays non-custodial. The taker chooses whether to take it.

## Working rules

- **Phase 0 (research) and Phase 1 (design) are documents only. Stop after Phase 1 and wait for the user's review before any code.**
- Don't guess APIs, contract addresses, fees, curve maths or chain behaviour: verify them against official docs, source code or the
  chains themselves, and cite them. Mark anything unverified **UNVERIFIED**.
- When a decision is the user's, list the options with their trade-offs instead of picking silently.
- Small, auditable code over features. Tests for every component, including adversarial ones.
- Never generate, request or handle mainnet keys. The user deploys contracts and funds makers. Tests stay on test networks.
- Read liq2sol's `CLAUDE.md` and its "Design points that are easy to get wrong". Most of them carry over, and several were learned the
  hard way.

## What to reuse from liq2sol (read these first)

- `docs/confidential.md`: the swap design. The MuSig2 taproot output, adapter-signed claims, blinded coins, the Simplicity refund leaf.
- `docs/protocol.md`: the protocol, decisions D1–D10, early refunds, and cancelling before anything is paid or locked.
- `docs/maker.md`, `docs/threat-model.md`, `docs/mainnet.md`, `docs/nostr-rfq.md`.
- Code: `contracts/swapkey` (Liquid side, reusable as is), `programs/solana-escrow` (the escrow to port to Polygon), `maker/`
  (daemon, signer, engine), `shared/rfq`, `client/` (the page).
- The release, signing, reproducible-build and nsite tooling: `scripts/`, `.github/workflows/`, `client/reproducible/`.

## Phase 0: research (documents only)

Write `docs/assumptions.md`, with a source for every finding and an UNVERIFIED list.

1. **USDT0 on Polygon PoS:** the token contract, its decimals, who can upgrade or freeze it, and whether it supports EIP-2612
   `permit` or something like it (for approvals without a separate transaction).
2. **The secret check on Polygon.** The Solana escrow pays out to whoever reveals the swap's secret, which must match its 64-byte
   `lock_point`; the Liquid side is bound to the same secret by the adapter signature. Find out exactly what liq2sol's escrow checks
   (`programs/solana-escrow`, `contracts/swapkey`'s `solana_point_of`), then whether a Polygon contract can check the same thing
   cheaply. The usual way is the `ecrecover` trick for secp256k1 scalar multiplication. Measure its gas.
3. **Polygon PoS as a chain for the short or long leg:** finality (Heimdall milestones), the history of reorganisations and halts,
   block time, and what that means for deadlines. liq2sol's halt rules depend on which chain holds the short leg ("Halts are
   asymmetric" in liq2sol's CLAUDE.md). Do the same analysis for Liquid ↔ Polygon in both directions.
4. **Gas for the taker on Polygon:** a taker needs POL to lock or claim. Options to compare: the taker holds POL; the maker pays
   (a relayer, or a claim the maker sends for the taker); permit plus a meta-transaction.
5. **The onward routes from Polygon, priced on mainnet:** Polygon ↔ each target (Solana, Tron, Ethereum, Arbitrum, Base…).
   - The real cost: LayerZero's `quoteSend`, the Legacy Mesh's 0.03%, and gas.
   - The time each takes, and the capacity limits (the mesh's "credits").
   - Who can stop or cancel a transfer.
   - sol2tron found the mesh's message fee large for some routes, about $11 Solana → Tron, so price before designing around it.
6. **Wallets:** an EVM wallet in a static page, through WalletConnect and browser extensions (eth2base's `app.js` is the reference),
   alongside the Solana and Liquid wallets the page already handles.
7. **Compare honestly with the alternative** of makers holding inventory on each target chain, with one escrow per chain, which keeps
   every leg atomic. Cost per swap, capital needed, trust, and how much code.

## Phase 1: design (documents only)

Write `docs/design.md`, then stop for the user's review. It should cover:

- **The Polygon escrow contract:** its rules, deadlines, claim and refund, halt handling, and tests. Whether it is immutable
  (liq2sol's Solana escrow is), and the per-swap cap on mainnet.
- **The maker:** Polygon in the daemon and the signer (keys, limits, what the signer refuses), inventory and pricing, and how it
  rebalances between Liquid and Polygon.
- **The RFQ protocol's next version:** directions as pairs of chains.
- **The page:**
  - every route a taker can pick, and which legs of it are atomic and which trust a bridge, said plainly before they commit;
  - recovery when an onward bridge leg is slow or stuck.
- **Decisions for the user**, with trade-offs:
  - which target chains first;
  - taker gas on Polygon;
  - an immutable contract;
  - the cap;
  - the name and repository;
  - whether liq2sol's Liquid ↔ Solana route stays separate or merges in.

## How it fits with SwapMarket's other sites

- **Licence:** liq2sol's split, kept here until the user says otherwise. `client/` is AGPL-3.0-only, because it copies AGPL code from
  `SwapMarket/swapmarket.github.io`; everything else is MIT, so makers can change the daemon without publishing it. Never copy AGPL code
  outside `client/`.
- **Releases and the page:** releases are built from signed tags and signed by hand, and the page is built reproducibly, with a
  `check-page.sh`. Both are published to GitHub Pages and as an nsite. Copy liq2sol's tooling.
- **Menus:** sites share a menu (`data-site` links through `siteUrl()`) and a footer link to the Nostr copy (`NSITE_GATEWAY`, nwb.tf).
  See liq2sol's `client/site/js/sites.js`.
- **WalletConnect:** one client per page (liq2sol's `wallet.js`), and Trust Wallet gets v0 Solana transactions (`txformat.js`).
  Both were bugs on mainnet.
