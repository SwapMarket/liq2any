# Code copied from liq2sol

liq2any copies liq2sol's code instead of depending on it (user's decision, 2026-11-22), so changes here never touch liq2sol's live
repository. This file records each copy, so it can be compared with liq2sol later.

| Here | From `SwapMarket/liq2sol` | At commit | Changes since copying |
|---|---|---|---|
| `contracts/swapkey/` | `contracts/swapkey/` | `75d1722` | Crate renamed `liq2any-swapkey`, and its tests' imports to match. Added `polygon_address_of`, `polygon_value`, `secret_from_polygon` and `tests/polygon.rs`, with `sha3 = "=0.12.0"` for keccak. `refund.simf` is unchanged, byte for byte. |
| `shared/chain/src/{liquid,esplora,swap}.rs` | `shared/chain/src/` | `75d1722` | Crate renamed `liq2any-chain`. `check_genesis` takes Polygon's chain id instead of Solana's genesis hash, with `POLYGON_MAINNET_CHAIN_ID` in place of `SOLANA_MAINNET_GENESIS`. Comments name Polygon. `sol.rs` is not copied; `evm.rs` and `polygon.rs` are new. |
| `shared/chain/tests/{backoff,confidential,forged_asset}.rs` | `shared/chain/tests/` | `75d1722` | Imports renamed. `network.rs` is rewritten for Polygon; the Solana-only tests are not copied. |
| `Cargo.lock` | `maker/Cargo.lock` | `75d1722` | Started from the maker's lock, a superset, so shared dependencies keep liq2sol's versions. Updated by cargo as crates were renamed, added (`sha3`, `secp256k1`'s `recovery` feature) or dropped (Solana). |
| `deny.toml` | `deny.toml` | `75d1722` | None |

The copied code keeps liq2sol's formatting, including the nine places where `cargo fmt --check` disagrees with it there too, so a
diff against liq2sol shows only real changes.

Licences: everything above is MIT in liq2sol and stays MIT here. Nothing from liq2sol's `client/` (AGPL) is copied outside
`client/`.
