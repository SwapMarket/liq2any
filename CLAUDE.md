# liq2any

Swap USDt on Liquid with USDT on other chains. The maker holds only Liquid USDt and USDT0 on Polygon PoS; the core swap, Liquid ↔
Polygon, is atomic and trustless, and other chains are reached through USDT0's bridge by the taker's own wallet. It grows out of
liq2sol (`~/src/liq2sol`).

**Read `docs/prompt.md` first.** It is the brief: the idea, what is known, and the phases. It governs the work.

## Rules

- **Phases 0 and 1 are documents only. Stop after Phase 1 and wait for the user's review before any code.**
- Don't guess APIs, contract addresses, fees or chain behaviour: verify them and cite sources. Mark anything unverified **UNVERIFIED**.
- When a decision is the user's, list the options with trade-offs instead of picking silently.
- Never generate, request or handle mainnet keys. The user deploys and funds.
- Small, auditable code over features. Every component needs tests, including adversarial ones.
- Licences: `client/` AGPL-3.0-only (`client/LICENSE`), everything else MIT (`LICENSE`), as in liq2sol. Never copy AGPL code outside
  `client/`.

## Repo conventions

- Commits are signed by SwapMarket (repo-local git config: user `SwapMarket`, key `01151A25AAE22F55`). Work on `main`. Commit
  when it makes sense, but **ask before each push**. The user approves a push by saying "push"; an "ok" or "yes" to something else
  is not enough.
- End commit messages with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Public repo: https://github.com/SwapMarket/liq2any (`origin`, branch `main`), created on 2026-11-22 (user's decision).
- Show the user screenshots of visual changes before committing them.

## Environment

- The same machine and tools as liq2sol. Read liq2sol's `CLAUDE.md` "Environment notes":
  - `RUSTFLAGS` breaks wasm32 and SBF builds here;
  - never kill `elementsd` by name;
  - Node: `export PATH="$HOME/.nvm/versions/node/v24.12.0/bin:$PATH"`;
  - Foundry is installed, for the Polygon contract.
- Sister repositories to read and copy from:
  - `~/src/liq2sol` (the swap itself);
  - `~/src/sol2tron` (Legacy Mesh research and code);
  - `~/src/eth2base` (EVM wallets in a static page);
  - `~/src/sol2base`;
  - `~/src/swapmarket.github.io` (BTC Swaps).
- The sol2tron conversation works in its own folder. Don't edit another project's working tree while its own conversation may be
  active in it.
