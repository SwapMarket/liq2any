# Code copied from liq2sol

liq2any copies liq2sol's code instead of depending on it (user's decision, 2026-11-22), so changes here never touch liq2sol's live
repository. This file records each copy, so it can be compared with liq2sol later.

| Here | From `SwapMarket/liq2sol` | At commit | Changes since copying |
|---|---|---|---|
| `contracts/swapkey/` | `contracts/swapkey/` | `75d1722` | Crate renamed `liq2any-swapkey`, and its tests' imports to match. Added `polygon_address_of`, `polygon_value`, `secret_from_polygon` and `tests/polygon.rs`, with `sha3 = "=0.12.0"` for keccak. `refund.simf` is unchanged, byte for byte. |
| `Cargo.lock` | `Cargo.lock` | `75d1722` | Updated by cargo for the rename and `sha3`. |
| `deny.toml` | `deny.toml` | `75d1722` | None |

The copied code keeps liq2sol's formatting, including the nine places where `cargo fmt --check` disagrees with it there too, so a
diff against liq2sol shows only real changes.

Licences: everything above is MIT in liq2sol and stays MIT here. Nothing from liq2sol's `client/` (AGPL) is copied outside
`client/`.
