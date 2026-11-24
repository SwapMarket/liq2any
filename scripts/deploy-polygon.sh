#!/usr/bin/env bash
# Deploys the escrow to Polygon PoS MAINNET, for Tether's USDT0, once (decision P3: immutable). Run by the user, with the user's own
# signer: forge's --ledger, --trezor, or --account NAME (a cast keystore, `cast wallet import NAME --interactive`). A key on the
# command line is refused, so it never lands in shell history.
#
#   scripts/deploy-polygon.sh --ledger
#   scripts/deploy-polygon.sh --account deployer
#
# RPC=… picks another endpoint (default https://polygon-bor-rpc.publicnode.com). Checks first, and asks before sending:
# the chain is 137, the tree is clean at a commit on origin/main, the contracts build, and the deployer has POL. Afterwards it
# checks the deployed code is this build's, the escrow names USDT0 and its block limits, and writes
# contracts/polygon/deployments/polygon.json.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/contracts/polygon"
RPC="${RPC:-https://polygon-bor-rpc.publicnode.com}"
OUT="$DIR/deployments/polygon.json"
USDT0=0xc2132D05D31c914a87C6611C10748AEb04B58e8F
# 14 days and 7 days at 1.5 s blocks: 1,500 ms a block, measured over 20,000 blocks on 2026-11-25 (docs/assumptions.md 3.1).
WINDOW_BLOCKS=806400
EXTENSION_BLOCKS=403200

die() { echo "$*" >&2; exit 1; }
[ $# -gt 0 ] || die "Name your signer: --ledger, --trezor, or --account NAME."
for a in "$@"; do
  case "$a" in --private-key|--private-key=*|--mnemonic*) die "No keys on the command line: use --ledger, --trezor or --account." ;; esac
done

[ "$(cast chain-id --rpc-url "$RPC")" = "137" ] || die "Not Polygon mainnet (chain 137). Refusing."
[ -e "$OUT" ] && die "$OUT exists: the escrow is deployed already, and it never changes."
[ -z "$(git -C "$ROOT" status --porcelain -- contracts/polygon)" ] || die "contracts/polygon has uncommitted changes. Deploy only committed code."
COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
git -C "$ROOT" fetch -q origin main
git -C "$ROOT" merge-base --is-ancestor "$COMMIT" origin/main || die "$COMMIT isn't on origin/main: push it first, so anyone can check the build."
[ "$(cast code "$USDT0" --rpc-url "$RPC" | wc -c)" -gt 100 ] || die "No USDT0 code at $USDT0."
(cd "$DIR" && forge build -q)

FROM="$(cd "$DIR" && cast wallet address "$@")"
BALANCE="$(cast balance "$FROM" --rpc-url "$RPC")"
echo "Deployer $FROM, $(cast from-wei "$BALANCE") POL"
echo "Escrow($USDT0, $WINDOW_BLOCKS, $EXTENSION_BLOCKS) from commit ${COMMIT:0:12}, about 1.6 M gas at $(cast from-wei "$(cast gas-price --rpc-url "$RPC")" gwei) gwei."
read -r -p "Deploy to Polygon mainnet? Type DEPLOY: " answer
[ "$answer" = DEPLOY ] || die "Nothing sent."

out="$(cd "$DIR" && forge create src/Escrow.sol:Escrow --rpc-url "$RPC" --broadcast --json "$@" --constructor-args "$USDT0" "$WINDOW_BLOCKS" "$EXTENSION_BLOCKS")"
read -r ESCROW ESCROW_TX <<<"$(python3 -I -c 'import json,sys; d=json.loads(sys.argv[1]); print(d["deployedTo"], d["transactionHash"])' "$out")"
[ -n "$ESCROW" ] || die "The deployment failed: $out"
echo "Escrow $ESCROW ($ESCROW_TX)"

# Every differing byte of deployed code must be a zero in the build: an immutable, filled in at deployment.
built="$(cd "$DIR" && forge inspect src/Escrow.sol:Escrow deployedBytecode)"
python3 -I -c '
import sys
a, b = bytes.fromhex(sys.argv[1][2:]), bytes.fromhex(sys.argv[2][2:])
sys.exit(0 if len(a) == len(b) and all(x == y or y == 0 for x, y in zip(a, b)) else 1)' "$(cast code "$ESCROW" --rpc-url "$RPC")" "$built" \
  || die "The deployed code differs from the build. Don't use $ESCROW."
lower() { tr 'A-F' 'a-f'; }
[ "$(cast call "$ESCROW" 'token()(address)' --rpc-url "$RPC" | lower)" = "$(echo "$USDT0" | lower)" ] || die "The escrow names the wrong token."
[ "$(cast call "$ESCROW" 'maxWindowBlocks()(uint256)' --rpc-url "$RPC" | cut -d' ' -f1)" = "$WINDOW_BLOCKS" ] || die "Wrong maxWindowBlocks."
[ "$(cast call "$ESCROW" 'maxExtensionBlocks()(uint256)' --rpc-url "$RPC" | cut -d' ' -f1)" = "$EXTENSION_BLOCKS" ] || die "Wrong maxExtensionBlocks."
echo "Deployed code matches the build, and the escrow names USDT0 and its block limits."

BLOCK="$(cast receipt "$ESCROW_TX" blockNumber --rpc-url "$RPC")"
mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<EOF
{
  "network": "polygon",
  "chain_id": 137,
  "token": "$USDT0",
  "escrow": "$ESCROW",
  "escrow_tx": "$ESCROW_TX",
  "escrow_block": $BLOCK,
  "max_window_blocks": $WINDOW_BLOCKS,
  "max_extension_blocks": $EXTENSION_BLOCKS,
  "deployer": "$FROM",
  "source_commit": "$COMMIT"
}
EOF
echo "Wrote $OUT. Tell Claude the escrow is deployed: it pins $ESCROW in the maker and the page."
