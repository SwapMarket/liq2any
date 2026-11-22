#!/usr/bin/env bash
# Deploys the stand-in token and the escrow to Polygon Amoy, TEST NETWORK ONLY, with the key in contracts/polygon/.keys/.
# Refuses any other chain. Writes the addresses to contracts/polygon/deployments/amoy.json.
#
#   scripts/deploy-amoy.sh [rpc]        (default https://polygon-amoy-bor-rpc.publicnode.com)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/contracts/polygon"
RPC="${1:-https://polygon-amoy-bor-rpc.publicnode.com}"
KEYFILE="$DIR/.keys/amoy-deployer.json"
OUT="$DIR/deployments/amoy.json"

[ "$(cast chain-id --rpc-url "$RPC")" = "80002" ] || { echo "Not Amoy (chain 80002). Refusing." >&2; exit 1; }
[ -f "$KEYFILE" ] || { echo "No key at $KEYFILE." >&2; exit 1; }
[ -e "$OUT" ] && { echo "$OUT exists: Amoy is already deployed. Move it away to deploy again." >&2; exit 1; }

KEY="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[0]["private_key"])' "$KEYFILE")"
FROM="$(cast wallet address --private-key "$KEY")"
echo "Deployer $FROM, balance $(cast from-wei "$(cast balance "$FROM" --rpc-url "$RPC")") POL"

deploy() {
  (cd "$DIR" && forge create "$@" --rpc-url "$RPC" --private-key "$KEY" --broadcast --json) |
    python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(d["deployedTo"], d["transactionHash"])'
}

read -r TOKEN TOKEN_TX < <(deploy src/testnet/StandInUSDT0.sol:StandInUSDT0)
echo "Stand-in token $TOKEN ($TOKEN_TX)"
# 14 days and 7 days at Amoy's 1.0 s blocks.
read -r ESCROW ESCROW_TX < <(deploy src/Escrow.sol:Escrow --constructor-args "$TOKEN" 1209600 604800)
echo "Escrow $ESCROW ($ESCROW_TX)"

BLOCK="$(cast receipt "$ESCROW_TX" blockNumber --rpc-url "$RPC")"
COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<EOF
{
  "network": "amoy",
  "chain_id": 80002,
  "token": "$TOKEN",
  "token_tx": "$TOKEN_TX",
  "escrow": "$ESCROW",
  "escrow_tx": "$ESCROW_TX",
  "escrow_block": $BLOCK,
  "max_window_blocks": 1209600,
  "max_extension_blocks": 604800,
  "deployer": "$FROM",
  "source_commit": "$COMMIT"
}
EOF
echo "Wrote $OUT"
