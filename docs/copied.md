# Code copied from liq2sol

liq2any copies liq2sol's code instead of depending on it (user's decision, 2026-11-22), so changes here never touch liq2sol's live
repository. This file records each copy, so it can be compared with liq2sol later.

| Here | From `SwapMarket/liq2sol` | At commit | Changes since copying |
|---|---|---|---|
| `contracts/swapkey/` | `contracts/swapkey/` | `75d1722` | Crate renamed `liq2any-swapkey`, and its tests' imports to match. Added `polygon_address_of`, `polygon_value`, `secret_from_polygon` and `tests/polygon.rs`, with `sha3 = "=0.12.0"` for keccak. `refund.simf` is unchanged, byte for byte. |
| `shared/chain/src/{liquid,esplora,swap}.rs` | `shared/chain/src/` | `75d1722` | Crate renamed `liq2any-chain`. `check_genesis` takes Polygon's chain id instead of Solana's genesis hash, with `POLYGON_MAINNET_CHAIN_ID` in place of `SOLANA_MAINNET_GENESIS`. Comments name Polygon. `sol.rs` is not copied; `evm.rs` and `polygon.rs` are new. |
| `shared/chain/tests/{backoff,confidential,forged_asset}.rs` | `shared/chain/tests/` | `75d1722` | Imports renamed. `network.rs` is rewritten for Polygon; the Solana-only tests are not copied. |
| `maker/core/src/{lib,store}.rs` | `maker/core/src/` | `75d1722` | Crate renamed `liq2any-maker-core`. The store drops the tables for closing Solana escrows and remembering vanished ones (an EVM lock never disappears), counts costs in POL gwei instead of lamports, and the signer's ledger gains a `nonces` table: one call per nonce, forever. |
| `maker/core/src/{engine,policy}.rs` | `maker/core/src/` | `75d1722` | Rewritten on the same structure for Polygon: both clocks on every lock, combined by leg; gates on both of the taker's clocks; finality lag and block rate in the health check; Tether's blocklist; the gas drop; `extend` on both clocks; the EVM nonce rule. |
| `maker/core/tests/` | `maker/core/tests/` | `75d1722` | Ported to Polygon case by case, with new cases for each rule above. `store.rs`'s two Solana-only tests are dropped. |
| `maker/signer/` | `maker/signer/` | `75d1722` | Crate renamed `liq2any-signer`. The Liquid half (wallet, fee coins, sessions, Liquid lock, claim and refund, withdrawals) is unchanged but for key-derivation tags, now `liq2any-…`, so keys never coincide with a liq2sol maker's. The Solana half is replaced by Polygon: locks, claims, refunds, `extend` and `release` through the escrow, the nonce rule with fee bumps, simulation before a nonce is taken, `ApproveEscrow` and `PendingPolygon`. The maker vault and escrow closing are gone. |
| `maker/signer/tests/process.rs` | same | `75d1722` | The same attacks through the socket, with a local `anvil` chain carrying the real escrow and stand-in token in place of Solana devnet, plus Polygon's: both clocks, the gas drop, the blocklist, approval, nonces and fee bumps. |
| `deploy/signer.example.json` | same | `75d1722` | Polygon settings, pointing at the Amoy deployment, in place of Solana's. |
| `Cargo.lock` | `maker/Cargo.lock` | `75d1722` | Started from the maker's lock, a superset, so shared dependencies keep liq2sol's versions. Updated by cargo as crates were renamed, added (`sha3`, `secp256k1`'s `recovery` feature) or dropped (Solana). |
| `deny.toml` | `deny.toml` | `75d1722` | None |

The copied code keeps liq2sol's formatting, including the nine places where `cargo fmt --check` disagrees with it there too, so a
diff against liq2sol shows only real changes.

Licences: everything above is MIT in liq2sol and stays MIT here. Nothing from liq2sol's `client/` (AGPL) is copied outside
`client/`.
