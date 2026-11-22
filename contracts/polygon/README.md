# Polygon escrow

The contract of `docs/design.md` section 2. Foundry, Solidity 0.8.30, EVM target Shanghai (Polygon's Bor rejects `BLOBBASEFEE`,
so nothing Cancun-specific is relied on).

| File | What |
|---|---|
| `src/Escrow.sol` | Locks, claims, refunds, releases and extensions. Immutable: no owner, no upgrade, no pause, no fee. |
| `src/SwapVault.sol` | Holds one swap's coins. `Escrow` deploys an EIP-1167 proxy of it per swap with CREATE2, at `vaultOf(id)`. |
| `src/TokenCall.sol` | ERC-20 calls that fail loudly |
| `test/vectors.txt` | Shared vectors from `scripts/polygon-vectors.py`: a pure-Python secp256k1 and keccak-256 |

## Tests

From this directory:

```bash
forge test                                                       # unit, adversarial and invariant suites; the fork suite skips
POLYGON_RPC=https://polygon.drpc.org forge test                  # also against the real USDT0, on a fork of Polygon mainnet
python3 -I ../../scripts/mutate-polygon.py                       # from the repo root: every rule removed in turn must fail a test
```

The fork needs an endpoint that serves state a few minutes old: publicnode refuses it, drpc serves it.

The fork suite checks the real token's behaviour that the design relies on:

- a permit over Polygon's salt domain;
- Tether's Safe (impersonated) blocking one vault, which stops that swap only;
- blocking `Escrow` itself, which stops new locks while open swaps still settle;
- burning a blocked vault's coins;
- a blocked recipient still being paid, which is why the page and the maker check `isBlocked` first.

## Gas

Measured on 2026-11-22 as separate transactions, on a fork of Polygon mainnet with the real USDT0 (`cast send` against `anvil`).
Dollars at 283.8 gwei and POL at $0.0757.

| Call | Gas | Cost |
|---|---|---|
| `approve` (USDT0) | 51,650 | $0.0011 |
| `lock`, taker's long leg | 217,122 | $0.0047 |
| `lock` with a gas drop, maker's short leg | 251,604 | $0.0054 |
| `claim`, to a recipient new to USDT0 | 106,888 | $0.0023 |

## Deploying

The constructor takes the token and the block counterparts of 14 days and 7 days for the network's block time:

- Polygon mainnet, 1.5 s blocks: `806400 403200`;
- Amoy, 1.0 s blocks: `1209600 604800`.

The user deploys. Mainnet is deployed once and never changed (decision P3).
